//! REST endpoints and the embedded frontend.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{
    ConnectInfo, DefaultBodyLimit, Extension, Path as AxPath, Query, Request, State,
};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::protocol::PermissionDecision;
use crate::agent::state::PermissionMode;
use crate::agent::supervisor::{DeleteError, ServerMsg, SpawnRequest, Supervisor};
use crate::config::Config;
use crate::remote::{Initiator, RemoteKey};
use crate::repo::{clone, git, scan};
use crate::uploads;

/// Events handed to a fresh page load before it starts streaming (§7).
pub const REPLAY_WINDOW: i64 = 500;

#[derive(rust_embed::Embed)]
#[folder = "src/assets/"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub sup: Arc<Supervisor>,
    pub config_path: PathBuf,
    /// The names we answer to, on the port we are listening on.
    pub hosts: Arc<HostPolicy>,
    /// The per-boot token a loopback client must carry (§7).
    pub token: Arc<SessionToken>,
    /// The durable key a paired device carries, once one has been paired (§12).
    pub remote: Option<Arc<RemoteKey>>,
    /// Throttle for the refusal log, so one stuck page cannot drown it.
    pub refusals: Arc<RefusalLog>,
}

/// The `Host` and `Origin` allowlist.
///
/// §7's rebinding defence requires a loopback `Host`, which a tailnet request
/// fails outright. Rebinding matters more remotely, not less, so the check
/// widens rather than lifting: loopback, the configured bind address and each
/// configured hostname, on the port actually being served. A hostile domain
/// rebound to your tailnet address still arrives with `Host: evil.example`,
/// which is on no list.
#[derive(Debug, Clone)]
pub struct HostPolicy {
    port: u16,
    names: Vec<String>,
}

impl HostPolicy {
    pub fn new(port: u16, bind: IpAddr, hostnames: &[String]) -> Self {
        let mut names = vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "[::1]".to_string(),
            "::1".to_string(),
        ];
        match bind {
            IpAddr::V4(v4) => names.push(v4.to_string()),
            IpAddr::V6(v6) => {
                names.push(v6.to_string());
                names.push(format!("[{v6}]"));
            }
        }
        names.extend(
            hostnames
                .iter()
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty()),
        );
        Self { port, names }
    }

    /// Hosts we answer to, on the port we are actually serving.
    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        let Some(host) = host else {
            // HTTP/1.1 requires a Host header; a request without one is not a
            // browser we want to trust.
            return false;
        };
        let host = host.trim();
        let (name, given_port) = match host.rsplit_once(':') {
            // An IPv6 literal keeps its brackets: `[::1]:7717`.
            Some((name, p)) if !name.ends_with('[') => (name, p.parse::<u16>().ok()),
            _ => (host, None),
        };
        let name = name.to_ascii_lowercase();
        let name_ok = self.names.contains(&name);
        let port_ok = match given_port {
            Some(p) => p == self.port,
            // A missing port means the scheme default.
            None => self.port == 80,
        };
        name_ok && port_ok
    }

    /// Origins we accept. Absent is fine — that is a non-browser client, which
    /// cannot be a rebinding victim; present and foreign is not.
    pub fn origin_allowed(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else {
            return true;
        };
        let origin = origin.trim();
        let Some(rest) = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
        else {
            // "null" and anything exotic is refused.
            return false;
        };
        self.host_allowed(Some(rest))
    }
}

/// How long one path's refusals collapse into a single log line.
const REFUSAL_QUIET: Duration = Duration::from_secs(60);

/// A rate limiter for "refused" log lines.
///
/// A refusal is worth logging: it is the only sign that something on this
/// machine is reaching for the control plane without the token. But a page
/// whose token died — the ordinary case being a tab left open across a restart,
/// since the token is minted per boot — retries its socket every 16s for as
/// long as it stays open, and two such tabs bury everything else in the log.
///
/// So the first is logged and the rest are counted: one line a minute per path,
/// carrying how many it stands for. Nothing is hidden — a flood still shows as
/// a flood, in one line instead of hundreds.
#[derive(Debug, Default)]
pub struct RefusalLog {
    seen: Mutex<HashMap<String, Refusal>>,
}

#[derive(Debug)]
struct Refusal {
    logged_at: Instant,
    since: u64,
}

impl RefusalLog {
    /// Record a refusal and log it if this path has been quiet long enough.
    ///
    /// A refusal from a non-loopback peer additionally records that peer's
    /// address and is throttled on its own key: someone on your tailnet failing
    /// to authenticate is worth more than a local page whose token went stale
    /// across a restart, and must not be swallowed by it. The credential that
    /// was presented is never logged, valid or not.
    pub fn note(&self, path: &str, peer: Option<SocketAddr>) {
        let remote = peer
            .filter(|p| !p.ip().is_loopback())
            .map(|p| p.ip().to_string());
        let key = match &remote {
            Some(ip) => format!("{path} from {ip}"),
            None => path.to_string(),
        };
        if let Some(suppressed) = self.tally(&key, Instant::now()) {
            if suppressed == 0 {
                tracing::warn!(%path, peer = ?remote, "refused a request with no valid credential");
            } else {
                tracing::warn!(
                    %path,
                    peer = ?remote,
                    suppressed,
                    "repeatedly refused requests with no valid credential; a page is likely \
                     retrying with a token from before the last restart, or a device needs \
                     pairing again"
                );
            }
        }
    }

    /// `Some(n)` if this refusal should be logged, where `n` is how many went
    /// unlogged since the last line for this path. `None` to stay quiet.
    fn tally(&self, path: &str, now: Instant) -> Option<u64> {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        match seen.get_mut(path) {
            Some(entry) if now.duration_since(entry.logged_at) < REFUSAL_QUIET => {
                entry.since += 1;
                None
            }
            Some(entry) => {
                let suppressed = std::mem::take(&mut entry.since);
                entry.logged_at = now;
                Some(suppressed)
            }
            None => {
                // A refused path is attacker-influenced only in so far as it
                // must be one we route; cap the map anyway rather than let it
                // grow for as long as the process lives.
                if seen.len() >= 64 {
                    seen.clear();
                }
                seen.insert(
                    path.to_string(),
                    Refusal {
                        logged_at: now,
                        since: 0,
                    },
                );
                Some(0)
            }
        }
    }
}

/// A random token minted at startup and handed to the browser in the URL the
/// server opens.
///
/// Loopback binding is not an authentication boundary: everything on the
/// machine can reach it, and that includes the agents themselves. §5 makes the
/// permission mode a control *over the agent*, so the endpoints that change it
/// must not be reachable by the agent — otherwise one approved Bash call is
/// enough for an agent to `POST /api/agents/<id>/permission_mode {"mode":
/// "bypass"}` and never be asked again.
///
/// **What this does and does not achieve.** It stops anything that has not been
/// handed the token: a drive-by cross-origin request, and any local process
/// that does not go looking for it. It does **not** make the token unreachable
/// to a determined process running as the same user, and nothing can: the
/// browser holds it in its profile, which is on disk and not privileged;
/// starting the browser puts the URL in another process's argv for a moment;
/// and a server run as `claude-web > log` writes it to that log. So this raises
/// the bar rather than closing the hole. The exposures the server itself
/// controls are kept small — the token is never logged through `tracing`, never
/// embedded in a served page, printed only to a terminal, and passed to the
/// browser through a private file rather than a command line — but an agent
/// that goes looking in the browser profile can still recover it.
pub struct SessionToken(String);

impl SessionToken {
    pub fn mint() -> Self {
        // 32 bytes straight from the OS: 256 bits, unlike two v4 UUIDs, which
        // carry 244 because 12 bits are fixed for version and variant.
        let mut bytes = [0u8; 32];
        if getrandom::fill(&mut bytes).is_err() {
            // The OS random source is not optional. Refusing to start beats
            // serving with a predictable token.
            panic!("the operating system random source is unavailable");
        }
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compare without leaking the answer through timing.
    pub fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let given = candidate.as_bytes();
        let mut diff = expected.len() ^ given.len();
        for (i, byte) in given.iter().enumerate() {
            diff |= usize::from(byte ^ expected.get(i).copied().unwrap_or(0));
        }
        diff == 0
    }
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let it reach a log through a derived Debug.
        f.write_str("SessionToken(<redacted>)")
    }
}

/// The header a browser sends the token in.
pub const TOKEN_HEADER: &str = "x-claude-web-token";

/// Anything an endpoint can refuse to do.
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    pub fn unauthorized(msg: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            body: json!({ "error": msg.to_string() }),
        }
    }

    pub fn bad_request(msg: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: json!({ "error": msg.to_string() }),
        }
    }

    pub fn not_found(msg: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: json!({ "error": msg.to_string() }),
        }
    }

    pub fn too_large(msg: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            body: json!({ "error": msg.to_string() }),
        }
    }

    pub fn conflict(body: Value) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            body,
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: json!({ "error": format!("{err:#}") }),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/agent/{slug}", get(agent_page))
        .route("/assets/{*path}", get(asset))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/repos", get(list_repos))
        .route("/api/repos/branches", get(repo_branches))
        .route("/api/repos/fetch", post(fetch_repo))
        .route("/api/repos/clone", post(clone_repo))
        .route("/api/agents", get(list_agents).post(spawn_agent))
        .route("/api/rate_limit", get(get_rate_limit))
        .route("/api/agents/{id}", get(get_agent).delete(delete_agent))
        .route("/api/agents/{id}/events", get(get_events))
        .route("/api/agents/{id}/message", post(post_message))
        .route("/api/agents/{id}/interrupt", post(interrupt_agent))
        .route("/api/agents/{id}/stop", post(stop_agent))
        .route("/api/agents/{id}/resume", post(resume_agent))
        .route("/api/agents/{id}/rename", post(rename_agent))
        .route(
            "/api/agents/{id}/permission_mode",
            post(set_permission_mode),
        )
        .route("/api/agents/{id}/permission", post(post_permission))
        .route("/api/agents/{id}/delete_preview", get(delete_preview))
        // The one route that takes a large body. axum's 2 MB default is lifted
        // here only; `upload_file` enforces `upload_max_mb` as it streams.
        .route(
            "/api/agents/{id}/uploads",
            get(list_uploads)
                .post(upload_file)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/agents/{id}/uploads/{name}",
            get(download_upload).delete(delete_upload),
        )
        .route("/api/notes", get(list_notes).post(create_note))
        .route("/api/notes/{id}", patch(update_note).delete(delete_note))
        .route("/ws", get(super::ws::handler))
        .route("/api/health", get(health))
        // Loopback binding alone does not survive DNS rebinding: a page on
        // http://evil.example:7717 rebound to 127.0.0.1 would otherwise reach
        // every endpoint here, including spawning an agent in `dangerous` mode.
        .layer(axum::middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Endpoints that carry data or change state. The pages and their assets stay
/// navigable so the browser can bootstrap; they contain nothing but markup.
pub fn requires_token(path: &str) -> bool {
    path.starts_with("/api/") || path == "/ws"
}

/// `Sec-Fetch-Site`, where the browser sends it. `same-origin` is our own page;
/// `none` is a typed URL or a bookmark. Anything else is another site asking.
pub fn fetch_site_allowed(site: Option<&str>) -> bool {
    match site {
        None => true,
        Some(site) => matches!(site.trim(), "same-origin" | "none"),
    }
}

/// The token a request carries: the header, or — only for the socket upgrade,
/// where a browser cannot set headers — the query string.
///
/// The query form is confined to `/ws` deliberately. Query strings end up in
/// logs, shell history and referrers, which is the kind of exposure the token
/// is trying to avoid.
fn token_of(req: &Request) -> Option<String> {
    if let Some(value) = req
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        return Some(value.trim().to_string());
    }
    if req.uri().path() != "/ws" {
        return None;
    }
    let query = req.uri().query()?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| value.trim().to_string())
    })
}

/// The address the request came from, as the listener saw it.
///
/// `None` only when the router was mounted without connect info, which the
/// server never does; it is read as "not loopback", so the per-boot token is
/// refused rather than accepted on a guess.
fn peer_of(req: &Request) -> Option<SocketAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
}

/// Which credential a request presented, if it presented a valid one.
///
/// Loopback and remote authenticate differently, and each is refused where it
/// does not belong. The per-boot token is delivered by strictly local means —
/// a URL the server opens on this machine — and therefore never legitimately
/// arrives from off-box; refusing it there costs one comparison and keeps §7's
/// one acknowledged leak (a server run as `claude-web > log`) a local problem
/// rather than a remotely replayable credential. The durable key is accepted
/// from any allowed peer, loopback included: the browser on this machine may
/// perfectly well be a paired device.
fn authenticate(state: &AppState, req: &Request, peer: Option<SocketAddr>) -> Option<Initiator> {
    let presented = token_of(req)?;
    let from_loopback = peer.is_some_and(|p| p.ip().is_loopback());
    if from_loopback && state.token.matches(&presented) {
        return Some(Initiator::Local);
    }
    let remote = state.remote.as_ref()?;
    remote.matches(&presented).then(|| Initiator::Paired {
        peer: peer.map_or_else(|| "unknown".to_string(), |p| p.ip().to_string()),
    })
}

/// What to tell a client whose credential did not work.
///
/// The stale-loopback-tab advice — "open the link claude-web printed" — is
/// useless on a phone that has never been near the terminal, so the message
/// branches on the peer address the server already knows.
fn refusal_message(from_loopback: bool) -> &'static str {
    if from_loopback {
        "This request needs the session token. Open the link claude-web printed at \
         startup — the token changes every time the server restarts."
    } else {
        "This device is not paired, or its key has been replaced. Run `claude-web pair` \
         on the machine running the server and scan the new code."
    }
}

async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let peer = peer_of(&req);
    let from_loopback = peer.is_some_and(|p| p.ip().is_loopback());
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !state.hosts.host_allowed(host) {
        tracing::warn!(?host, "refused a request with an unknown Host header");
        return (
            StatusCode::FORBIDDEN,
            "claude-web does not answer to that Host header",
        )
            .into_response();
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !state.hosts.origin_allowed(origin) {
        tracing::warn!(?origin, "refused a cross-origin request");
        return (
            StatusCode::FORBIDDEN,
            "claude-web refuses cross-origin requests",
        )
            .into_response();
    }

    let path = req.uri().path().to_string();
    if requires_token(&path) {
        // A cross-origin no-cors GET carries no `Origin` at all — an `<img>` or
        // `<script>` tag on any page reaches loopback with a loopback `Host` —
        // so absence of `Origin` cannot be read as "same origin". Two things
        // close it: the browser's own `Sec-Fetch-Site`, and the token, which
        // also keeps out non-browser callers on this machine, agents included.
        let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
        if !fetch_site_allowed(site) {
            tracing::warn!(?site, %path, "refused a cross-site request");
            return (
                StatusCode::FORBIDDEN,
                "claude-web refuses cross-site requests",
            )
                .into_response();
        }
        let Some(initiator) = authenticate(&state, &req, peer) else {
            state.refusals.note(&path, peer);
            return ApiError::unauthorized(refusal_message(from_loopback)).into_response();
        };
        // Which client asked. Endpoints that write into an agent's event log
        // read it back out of here (§12).
        req.extensions_mut().insert(initiator);
    }

    next.run(req).await
}

// -- static assets ----------------------------------------------------------

/// Everything is served from this origin, nothing may frame us, and no page
/// may navigate or post anywhere. Approve is a one-click destructive action, so
/// clickjacking matters here.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; font-src 'self'; connect-src 'self' ws: wss:; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'";

fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    (header::CONTENT_SECURITY_POLICY, CSP),
                    (header::X_FRAME_OPTIONS, "DENY"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::REFERRER_POLICY, "no-referrer"),
                    // The assets are compiled into the binary and their URLs
                    // carry no content hash, so `app.js` after an upgrade is a
                    // different file at the same address. With no directive at
                    // all a browser is free to heuristically cache it, which
                    // makes "reload to pick up the fix" a coin toss. They are
                    // a few KB over loopback: always revalidate.
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("no such asset: {path}")).into_response(),
    }
}

async fn index() -> Response {
    serve_embedded("index.html")
}

async fn agent_page(AxPath(_slug): AxPath<String>) -> Response {
    // The slug is resolved client-side from the URL; one shell serves them all.
    serve_embedded("agent.html")
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches("/assets/");
    serve_embedded(path)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

// -- config -----------------------------------------------------------------

async fn get_config(State(state): State<AppState>) -> Json<Config> {
    Json(state.sup.config().await)
}

async fn put_config(
    State(state): State<AppState>,
    Json(cfg): Json<Config>,
) -> ApiResult<Json<Config>> {
    // Where this thing listens is not editable through the control plane it
    // serves: a client that can widen its own listening address is a privilege
    // escalation with extra steps, and it is precisely the move a client that
    // had got hold of a credential would make (§12). Whatever the body claims,
    // the running values are kept.
    let current = state.sup.config().await;
    let cfg = Config {
        bind: current.bind,
        hostnames: current.hostnames,
        ..cfg
    };
    // The branch prefix reaches git as a positional argument, so an
    // option-shaped one is refused here rather than at spawn time.
    cfg.validate().map_err(ApiError::from)?;
    let path = state.config_path.clone();
    let to_save = cfg.clone();
    tokio::task::spawn_blocking(move || to_save.save(&path))
        .await
        .map_err(ApiError::bad_request)?
        .map_err(ApiError::from)?;
    state.sup.set_config(cfg.clone()).await;
    Ok(Json(cfg))
}

// -- repos ------------------------------------------------------------------

async fn list_repos(State(state): State<AppState>) -> ApiResult<Json<scan::RepoListing>> {
    let cfg = state.sup.config().await;
    let usage = state.sup.db().run(|db| db.repo_usage()).await?;
    let roots = cfg.roots();
    let listing = tokio::task::spawn_blocking(move || scan::scan_roots(&roots, &usage))
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(listing))
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
}

#[derive(Debug, Serialize)]
struct BranchInfo {
    branches: Vec<String>,
    current: Option<String>,
    dirty: bool,
    is_git: bool,
    /// The path is a configured root itself, so the spawn it describes is
    /// rootless (§6): no branch, no worktree, and `is_git` reported as false
    /// however the directory is laid out.
    is_root: bool,
}

/// Resolve a caller-supplied repo path, refusing anything outside the roots.
///
/// `git` honours the config of the directory it runs in, so an unconfined path
/// here is an arbitrary-command primitive, not merely an information leak.
async fn confined_repo(state: &AppState, path: &str) -> ApiResult<PathBuf> {
    let roots = state.sup.config().await.roots();
    let candidate = crate::config::expand_tilde(path);
    tokio::task::spawn_blocking(move || crate::config::confine_to_roots(&candidate, &roots))
        .await
        .map_err(ApiError::bad_request)?
        .map_err(ApiError::from)
}

async fn repo_branches(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<BranchInfo>> {
    let path = confined_repo(&state, &q.path).await?;
    let roots = state.sup.config().await.roots();
    let info = tokio::task::spawn_blocking(move || {
        // A root answers as a root and nothing else. Running git in it would
        // describe branches the spawn will not touch, and the form would offer
        // the operator choices nothing honours.
        if crate::config::is_configured_root(&path, &roots) {
            return BranchInfo {
                branches: Vec::new(),
                current: None,
                dirty: false,
                is_git: false,
                is_root: true,
            };
        }
        let is_git = git::is_git_repo(&path);
        BranchInfo {
            branches: if is_git {
                git::list_branches(&path)
            } else {
                Vec::new()
            },
            current: if is_git {
                git::current_branch(&path)
            } else {
                None
            },
            dirty: is_git && git::is_dirty(&path),
            is_git,
            is_root: false,
        }
    })
    .await
    .map_err(ApiError::bad_request)?;
    Ok(Json(info))
}

async fn fetch_repo(
    State(state): State<AppState>,
    Json(q): Json<PathQuery>,
) -> ApiResult<Json<Value>> {
    let path = confined_repo(&state, &q.path).await?;
    let output = tokio::task::spawn_blocking(move || git::fetch(&path))
        .await
        .map_err(ApiError::bad_request)?
        .map_err(ApiError::from)?;
    Ok(Json(json!({"ok": true, "output": output})))
}

#[derive(Debug, Deserialize)]
struct CloneRequest {
    url: String,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    /// Spawn an agent in the clone once it lands.
    #[serde(default)]
    spawn: Option<SpawnRequest>,
}

async fn clone_repo(
    State(state): State<AppState>,
    Json(req): Json<CloneRequest>,
) -> ApiResult<Json<Value>> {
    let cfg = state.sup.config().await;
    let root = match &req.root {
        // A caller-chosen root is confined to the configured ones: without
        // this, a clone writes anywhere on disk, creating parents as it goes.
        Some(r) => confined_repo(&state, r).await?,
        None => cfg
            .roots()
            .first()
            .cloned()
            .ok_or_else(|| ApiError::bad_request("no repo roots configured"))?,
    };
    let folder = req
        .folder
        .clone()
        .filter(|f| !f.trim().is_empty())
        .or_else(|| clone::folder_name_from_url(&req.url))
        .ok_or_else(|| ApiError::bad_request("could not derive a folder name from that URL"))?;
    // Fail fast on a bad URL or name before we tell the client the clone
    // started: an option-shaped URL is refused here, not asynchronously.
    clone::validate_url(&req.url).map_err(ApiError::from)?;
    clone::clone_destination(&root, &folder).map_err(ApiError::from)?;

    let clone_id = uuid::Uuid::new_v4().to_string();
    let sup = state.sup.clone();
    let id = clone_id.clone();
    let url = req.url.clone();
    let folder_for_task = folder.clone();
    tokio::spawn(async move {
        let progress_sup = sup.clone();
        let progress_id = id.clone();
        let result = clone::clone(&url, &root, &folder_for_task, move |line| {
            progress_sup.broadcast(ServerMsg::CloneProgress {
                clone_id: progress_id.clone(),
                line,
            });
        })
        .await;
        match result {
            Ok(outcome) => {
                tracing::debug!(output = %outcome.stderr, "git clone finished");
                let path = outcome.path.to_string_lossy().to_string();
                sup.broadcast(ServerMsg::CloneDone {
                    clone_id: id.clone(),
                    path: Some(path.clone()),
                    error: None,
                });
                if let Some(mut spawn) = req.spawn {
                    spawn.repo_path = path;
                    if let Err(err) = sup.spawn_agent(spawn).await {
                        sup.broadcast(ServerMsg::Notice {
                            agent_id: None,
                            level: "error".to_string(),
                            text: format!("Clone succeeded but the agent did not start: {err:#}"),
                        });
                    }
                }
            }
            Err(err) => sup.broadcast(ServerMsg::CloneDone {
                clone_id: id.clone(),
                path: None,
                error: Some(format!("{err:#}")),
            }),
        }
    });

    Ok(Json(json!({"clone_id": clone_id, "folder": folder})))
}

// -- agents -----------------------------------------------------------------

async fn list_agents(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let agents = state.sup.list().await?;
    Ok(Json(json!({ "agents": agents })))
}

/// The last rate-limit snapshot, so a page loaded between two events still has
/// numbers to show. `null` until some agent's CLI reports one.
async fn get_rate_limit(State(state): State<AppState>) -> Json<Value> {
    // `captured_at` travels with it: a restored snapshot can be hours old, and
    // a figure that stale has to be labelled rather than passed off as live.
    match state.sup.rate_limit().await {
        Some((captured_at, info)) => Json(json!({
            "rate_limit": info,
            "captured_at": captured_at,
        })),
        None => Json(json!({ "rate_limit": Value::Null, "captured_at": Value::Null })),
    }
}

async fn get_agent(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    // The composer's recall list (§7). It rides on the detail payload rather
    // than the agent view because only this page wants it: the dashboard
    // renders the same view for every agent at once, and would pay for a
    // history query per card to show none of them.
    let agent_id = record.id.clone();
    let history = state
        .sup
        .db()
        .run(move |db| db.recent_user_inputs(&agent_id))
        .await
        .map_err(ApiError::from)?;
    let view = state.sup.view(record).await?;
    Ok(Json(json!({ "agent": view, "input_history": history })))
}

/// Accept either an id or a slug, so `/agent/<slug>` pages can use one path.
async fn resolve(state: &AppState, id: &str) -> ApiResult<crate::db::AgentRecord> {
    let key = id.to_string();
    let by_id = state
        .sup
        .db()
        .run(move |db| db.get_agent(&key))
        .await
        .map_err(ApiError::from)?;
    if let Some(record) = by_id {
        return Ok(record);
    }
    let key = id.to_string();
    state
        .sup
        .db()
        .run(move |db| db.get_agent_by_slug(&key))
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("no such agent: {id}")))
}

async fn spawn_agent(
    State(state): State<AppState>,
    Json(req): Json<SpawnRequest>,
) -> ApiResult<Json<Value>> {
    let outcome = state.sup.spawn_agent(req).await?;
    Ok(Json(json!({
        "agent": outcome.agent,
        "warning": outcome.warning,
    })))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn get_events(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let limit = q.limit.unwrap_or(REPLAY_WINDOW).clamp(1, 5000);
    let agent_id = record.id.clone();
    let db = state.sup.db().clone();
    let after = match q.after {
        Some(after) => after,
        // A fresh load starts one window back from the head (§7).
        None => {
            let agent_id = agent_id.clone();
            db.run(move |db| db.tail_cursor(&agent_id, limit))
                .await
                .map_err(ApiError::from)?
        }
    };
    let events = db
        .run(move |db| db.events_after(&agent_id, after, limit))
        .await
        .map_err(ApiError::from)?;
    let cursor = events.last().map(|e| e.seq).unwrap_or(after);
    let agent_id = record.id.clone();
    let max_seq = db
        .run(move |db| db.max_seq(&agent_id))
        .await
        .unwrap_or(cursor);
    Ok(Json(json!({
        "agent_id": record.id,
        "after": after,
        "cursor": cursor,
        "has_more": cursor < max_seq,
        "events": events,
    })))
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    text: String,
    /// Names of pending uploads to attach (§7, "Attaching files").
    #[serde(default)]
    attachments: Vec<String>,
}

async fn post_message(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<MessageBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state
        .sup
        .send_message(&record.id, &body.text, &body.attachments)
        .await?;
    Ok(Json(json!({"ok": true})))
}

// -- uploads ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UploadQuery {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct ListUploadsQuery {
    #[serde(default)]
    pending: Option<String>,
}

/// An upload as the page sees it, with the path the agent will be given.
fn upload_json(state: &AppState, agent_id: &str, upload: &crate::db::Upload) -> Value {
    json!({
        "name": upload.name,
        "size": upload.size,
        "path": state.sup.uploads_dir(agent_id).join(&upload.name).to_string_lossy(),
        "created_at": upload.created_at,
        "sent_at": upload.sent_at,
    })
}

/// A stored name, or a 404. Anything `clean_name` would change was never
/// stored, so a name with a separator or a leading dot is refused before it
/// gets near a path.
fn stored_name(name: &str) -> ApiResult<&str> {
    if uploads::clean_name(name) != name {
        return Err(ApiError::not_found(format!("no such upload: {name}")));
    }
    Ok(name)
}

/// `POST /api/agents/{id}/uploads?name=<original>`: the raw body is the file.
///
/// Streamed to disk chunk by chunk and cut off at `upload_max_mb`; a refused
/// or abandoned upload leaves nothing behind. Allowed whatever the agent's
/// status — a stopped agent's attachments wait in the composer for Resume.
async fn upload_file(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    use tokio::io::AsyncWriteExt;

    let record = resolve(&state, &id).await?;
    let max_mb = state.sup.config().await.upload_max_mb;
    let max_bytes = max_mb.saturating_mul(1024 * 1024);
    let too_large = || ApiError::too_large(format!("files are limited to {max_mb} MB"));
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max_bytes) {
        return Err(too_large());
    }

    let dir = state.sup.uploads_dir(&record.id);
    let wanted = uploads::clean_name(&q.name);
    let dir_for_create = dir.clone();
    let wanted_again = uploads::clean_name(&q.name);
    let (file, mut name, mut suffix) = tokio::task::spawn_blocking(move || {
        uploads::ensure_dir(&dir_for_create)?;
        uploads::create_unique(&dir_for_create, &wanted, 1)
    })
    .await
    .map_err(ApiError::bad_request)?
    .map_err(ApiError::from)?;
    // Removes the file unless the upload completes. The handler future is
    // simply dropped when the client aborts or the connection goes, so the
    // cleanup has to live in a destructor, not after an `.await`.
    let mut partial = PartialUpload {
        path: dir.join(&name),
        keep: false,
    };

    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut size: u64 = 0;
    let mut failure: Option<ApiError> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                failure = Some(ApiError::bad_request(format!(
                    "the upload was cut off: {err}"
                )));
                break;
            }
        };
        size += chunk.len() as u64;
        if size > max_bytes {
            failure = Some(too_large());
            break;
        }
        if let Err(err) = file.write_all(&chunk).await {
            failure = Some(ApiError::bad_request(format!("writing the upload: {err}")));
            break;
        }
    }
    if failure.is_none()
        && let Err(err) = file.flush().await
    {
        failure = Some(ApiError::bad_request(format!("writing the upload: {err}")));
    }
    drop(file);
    if let Some(err) = failure {
        return Err(err);
    }

    // The row is what makes the name ours. If one already holds it — an
    // earlier upload whose file the agent moved away — the file moves on to
    // the next free name and tries again; plain INSERT makes that race-free.
    let upload = loop {
        let agent_id = record.id.clone();
        let stored = name.clone();
        let inserted = state
            .sup
            .db()
            .run(move |db| db.insert_upload(&agent_id, &stored, size))
            .await?;
        if let Some(upload) = inserted {
            break upload;
        }
        let dir_for_move = dir.clone();
        let wanted = wanted_again.clone();
        let from = partial.path.clone();
        let (next, next_suffix) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let (placeholder, next, n) =
                uploads::create_unique(&dir_for_move, &wanted, suffix + 1)?;
            drop(placeholder);
            let to = dir_for_move.join(&next);
            // Onto our own empty placeholder; a link planted there in the
            // meantime is replaced, never followed.
            if let Err(err) = std::fs::rename(&from, &to) {
                std::fs::remove_file(&to).ok();
                return Err(err.into());
            }
            Ok((next, n))
        })
        .await
        .map_err(ApiError::bad_request)?
        .map_err(ApiError::from)?;
        partial.path = dir.join(&next);
        name = next;
        suffix = next_suffix;
    };
    partial.keep = true;
    Ok(Json(upload_json(&state, &record.id, &upload)))
}

/// An upload file that is deleted when dropped, unless `keep` was set once
/// its row was recorded. Without it an aborted upload leaves a file with no
/// row: invisible, undeletable, and holding its name.
struct PartialUpload {
    path: std::path::PathBuf,
    keep: bool,
}

impl Drop for PartialUpload {
    fn drop(&mut self) {
        if !self.keep {
            std::fs::remove_file(&self.path).ok();
        }
    }
}

/// `GET /api/agents/{id}/uploads[?pending=1]`. With `pending`, only what is
/// still waiting in the composer — what a reload puts back as chips.
async fn list_uploads(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<ListUploadsQuery>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let pending = q
        .pending
        .as_deref()
        .is_some_and(|v| !matches!(v, "" | "0" | "false"));
    let agent_id = record.id.clone();
    let rows = state
        .sup
        .db()
        .run(move |db| db.list_uploads(&agent_id, pending))
        .await?;
    let list: Vec<Value> = rows
        .iter()
        .map(|u| upload_json(&state, &record.id, u))
        .collect();
    Ok(Json(json!({ "uploads": list })))
}

/// `GET /api/agents/{id}/uploads/{name}`: always a download, never rendered.
///
/// The agent can write in its upload folder, so the entry is refused unless
/// it is still a plain file — a symlink planted in place of an upload would
/// otherwise hand out whatever it points at.
async fn download_upload(
    State(state): State<AppState>,
    AxPath((id, name)): AxPath<(String, String)>,
) -> ApiResult<Response> {
    let record = resolve(&state, &id).await?;
    let name = stored_name(&name)?.to_string();
    let agent_id = record.id.clone();
    let key = name.clone();
    if state
        .sup
        .db()
        .run(move |db| db.get_upload(&agent_id, &key))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!("no such upload: {name}")));
    }
    let dir = state.sup.uploads_dir(&record.id);
    let key = name.clone();
    let (file, len) = tokio::task::spawn_blocking(move || uploads::open_plain_file(&dir, &key))
        .await
        .map_err(ApiError::bad_request)?
        .map_err(|err| ApiError::not_found(format!("{name} cannot be downloaded: {err:#}")))?;
    // Streamed in chunks, and no further than the length it had when opened:
    // the agent may be growing it, and the length is what was promised.
    let reader = tokio::io::AsyncReadExt::take(tokio::fs::File::from_std(file), len);
    let chunks = futures_util::stream::unfold(Some(reader), |reader| async move {
        use tokio::io::AsyncReadExt;
        let mut reader = reader?;
        let mut buf = vec![0u8; 64 * 1024];
        match reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(axum::body::Bytes::from(buf)), Some(reader)))
            }
            Err(err) => Some((Err(err), None)),
        }
    });
    Ok((
        [
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                uploads::content_disposition(&name),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox".to_string(),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from_stream(chunks),
    )
        .into_response())
}

/// `DELETE /api/agents/{id}/uploads/{name}`: withdraw a pending upload. A
/// sent one is part of the transcript and is refused.
async fn delete_upload(
    State(state): State<AppState>,
    AxPath((id, name)): AxPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let name = stored_name(&name)?.to_string();
    let agent_id = record.id.clone();
    let key = name.clone();
    let removed = state
        .sup
        .db()
        .run(move |db| {
            let row = db.get_upload(&agent_id, &key)?;
            if row.as_ref().is_some_and(|r| r.sent_at.is_some()) {
                return Ok(None);
            }
            Ok(Some(db.delete_pending_upload(&agent_id, &key)?))
        })
        .await?;
    match removed {
        None => Err(ApiError::conflict(
            json!({ "error": format!("{name} has already been sent") }),
        )),
        Some(false) => Err(ApiError::not_found(format!("no such upload: {name}"))),
        Some(true) => {
            // `remove_file` takes a link away rather than what it points at.
            let path = state.sup.uploads_dir(&record.id).join(&name);
            tokio::fs::remove_file(&path).await.ok();
            Ok(Json(json!({"ok": true})))
        }
    }
}

async fn interrupt_agent(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state.sup.interrupt(&record.id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn stop_agent(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state.sup.stop(&record.id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn resume_agent(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state.sup.resume(&record.id).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Debug, Deserialize)]
struct RenameBody {
    name: String,
}

async fn rename_agent(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let updated = state.sup.rename(&record.id, &body.name).await?;
    Ok(Json(json!({ "agent": updated })))
}

#[derive(Debug, Deserialize)]
struct PermissionModeBody {
    mode: PermissionMode,
    /// Set by the UI once the operator has confirmed a change that gives the
    /// agent more freedom.
    #[serde(default)]
    confirm: bool,
}

/// Relaxing a permission mode is something a paired device may do, like
/// everything else (§12): a client that can approve each individual tool call
/// already reaches everywhere bypass mode reaches, one prompt at a time, and
/// unblocking a stuck agent from a phone is the reason to want this at all.
/// Which client asked is recorded in the agent's own log.
async fn set_permission_mode(
    State(state): State<AppState>,
    Extension(initiator): Extension<Initiator>,
    AxPath(id): AxPath<String>,
    Json(body): Json<PermissionModeBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state
        .sup
        .set_permission_mode(&record.id, body.mode, body.confirm, initiator)
        .await?;
    Ok(Json(json!({"ok": true, "mode": body.mode})))
}

#[derive(Debug, Deserialize)]
struct PermissionBody {
    request_id: String,
    behavior: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    updated_input: Option<Value>,
}

async fn post_permission(
    State(state): State<AppState>,
    Extension(initiator): Extension<Initiator>,
    AxPath(id): AxPath<String>,
    Json(body): Json<PermissionBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let decision = decision_from(&body.behavior, body.message, body.updated_input)
        .ok_or_else(|| ApiError::bad_request(format!("unknown behavior: {}", body.behavior)))?;
    state
        .sup
        .decide(&record.id, &body.request_id, decision, initiator)
        .await?;
    Ok(Json(json!({"ok": true})))
}

/// Map the wire `behavior` string onto a decision.
pub fn decision_from(
    behavior: &str,
    message: Option<String>,
    updated_input: Option<Value>,
) -> Option<PermissionDecision> {
    match behavior {
        "allow" => Some(PermissionDecision::Allow { updated_input }),
        "deny" => Some(PermissionDecision::Deny { message }),
        _ => None,
    }
}

async fn delete_preview(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let report = state.sup.delete_preview(&record.id).await?;
    Ok(Json(json!({ "report": report })))
}

#[derive(Debug, Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    delete_branch: bool,
}

async fn delete_agent(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    match state.sup.delete(&record.id, q.force, q.delete_branch).await {
        Ok(()) => Ok(Json(json!({"ok": true}))),
        Err(DeleteError::Unsafe(refusal)) => Err(ApiError::conflict(json!({
            "error": refusal.message,
            "report": refusal.report,
        }))),
        Err(DeleteError::Other(msg)) => Err(ApiError::bad_request(msg)),
    }
}

// -- notes -------------------------------------------------------------------

/// A body big enough for any memo and small enough that the dashboard's poll
/// can carry the whole list without thinking about it.
const MAX_NOTE_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
struct NoteBody {
    body: String,
}

/// Surrounding whitespace is never part of a memo, and a note that is only
/// whitespace is not one — refused on create and on edit alike, so the database
/// never holds a blank row.
fn clean_note(body: &str) -> ApiResult<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("a note needs a body"));
    }
    if trimmed.len() > MAX_NOTE_BYTES {
        return Err(ApiError::bad_request(format!(
            "a note is limited to {MAX_NOTE_BYTES} bytes"
        )));
    }
    Ok(trimmed.to_string())
}

async fn list_notes(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let notes = state.sup.db().run(|db| db.list_notes()).await?;
    Ok(Json(json!({ "notes": notes })))
}

async fn create_note(
    State(state): State<AppState>,
    Json(body): Json<NoteBody>,
) -> ApiResult<Json<Value>> {
    let body = clean_note(&body.body)?;
    let note = state.sup.db().run(move |db| db.create_note(&body)).await?;
    Ok(Json(json!({ "note": note })))
}

async fn update_note(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<NoteBody>,
) -> ApiResult<Json<Value>> {
    let body = clean_note(&body.body)?;
    let key = id.clone();
    let note = state
        .sup
        .db()
        .run(move |db| db.update_note(&key, &body))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no such note: {id}")))?;
    Ok(Json(json!({ "note": note })))
}

async fn delete_note(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let key = id.clone();
    if !state.sup.db().run(move |db| db.delete_note(&key)).await? {
        return Err(ApiError::not_found(format!("no such note: {id}")));
    }
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_the_pages_reference_is_embedded() {
        for name in [
            "index.html",
            "agent.html",
            "app.css",
            "common.js",
            "dashboard.js",
            "agent.js",
            "favicon.svg",
            "favicon-alert.svg",
            "favicon-flash.svg",
            "favicon-done.svg",
            "attention.js",
            "splitter.js",
            "uploads.js",
        ] {
            assert!(
                Assets::get(name).is_some(),
                "missing embedded asset: {name}"
            );
        }
    }

    /// Drive the real `attention.js` — the tab alert's decisions, kept free of
    /// the DOM and Web Audio for exactly this — through the cases that matter:
    /// which of several tabs chimes, and which half the flash starts on.
    #[test]
    fn the_tab_alert_chimes_once_per_request_and_flashes_orange_first() {
        let module =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets/attention.js");
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = dir.path().join("attention.mjs");
        let source = format!(
            r#"
import {{ Chimer, ChimeClaims, DoneTracker, DONE_SETTLE_MS, Flasher, PendingRequests, Resync, withDeadline, attentionKey, doneKey, finishedTurn, newKeys, tabLook, ICON, ICON_ALERT, ICON_FLASH, ICON_DONE_FLASH, CHIME_CLAIM_MS, CHIME_STORAGE_KEY }} from "{module}";

const assert = (cond, msg) => {{ if (!cond) {{ console.error("FAIL: " + msg); process.exit(1); }} }};

// One localStorage, shared by every tab of the origin.
const storage = () => {{
  const map = new Map();
  return {{ getItem: (k) => (map.has(k) ? map.get(k) : null), setItem: (k, v) => map.set(k, String(v)), map }};
}};
let clock = 1_000_000;
const now = () => clock;

// A tab: its own audio context, the shared storage, a count of what it played.
const tab = (shared, state) => {{
  const ctx = {{ state, resumed: 0, resume() {{ this.resumed += 1; return Promise.resolve(); }} }};
  const t = {{ ctx, played: 0 }};
  t.chimer = new Chimer({{ context: () => ctx, claims: new ChimeClaims(shared, now), play: () => {{ t.played += 1; }} }});
  return t;
}};

// A locked tab seeing the request first must not claim it away from an
// unlocked one: exactly one chime, from the tab that can play.
{{
  const shared = storage();
  const locked = tab(shared, "suspended");
  const open = tab(shared, "running");
  const key = attentionKey("a", "r1");
  locked.chimer.announce(key);
  open.chimer.announce(key);
  assert(locked.played === 0, "a locked tab must stay silent");
  assert(open.played === 1, "the unlocked tab must chime: " + open.played);
  await Promise.resolve();
  assert(locked.ctx.resumed === 1, "the locked tab should ask to resume");
}}

// Interleaved requests across two tabs: r2 must not overwrite r1's claim.
{{
  const shared = storage();
  const one = tab(shared, "running");
  const two = tab(shared, "running");
  const r1 = attentionKey("a", "r1");
  const r2 = attentionKey("a", "r2");
  one.chimer.announce(r1);
  one.chimer.announce(r2);
  two.chimer.announce(r1);
  two.chimer.announce(r2);
  assert(one.played === 2, "the first tab chimes for each request: " + one.played);
  assert(two.played === 0, "the second tab must not chime again: " + two.played);
}}

// Request ids are only unique per agent: the same id from two agents is two
// requests, and both chime.
{{
  const shared = storage();
  const only = tab(shared, "running");
  only.chimer.announce(attentionKey("a", "1"));
  only.chimer.announce(attentionKey("b", "1"));
  assert(only.played === 2, "two agents, one id: both must chime: " + only.played);
}}

// A throttled background tab reaches the same request a minute or more
// later, after a reconnect: still one chime.
{{
  const shared = storage();
  const one = tab(shared, "running");
  const late = tab(shared, "running");
  one.chimer.announce(attentionKey("a", "r1"));
  clock += 90_000;
  late.chimer.announce(attentionKey("a", "r1"));
  assert(late.played === 0, "a late tab must not chime again");
  assert(CHIME_CLAIM_MS >= 5 * 60_000, "the claim must outlast a throttled reconnect");
}}

// Claims expire, and the map is pruned on write so it stays bounded.
{{
  const shared = storage();
  const only = tab(shared, "running");
  for (let i = 0; i < 50; i += 1) {{
    only.chimer.announce(attentionKey("a", String(i)));
    clock += 60_000;
  }}
  const held = Object.keys(JSON.parse(shared.map.get(CHIME_STORAGE_KEY)));
  assert(held.length <= 11, "stale claims must be pruned: " + held.length);
}}

// An answered request frees its id: a later request reusing it chimes.
{{
  const shared = storage();
  const only = tab(shared, "running");
  const claims = new ChimeClaims(shared, now);
  only.chimer.announce(attentionKey("a", "1"));
  claims.release(attentionKey("a", "1"));
  only.chimer.announce(attentionKey("a", "1"));
  assert(only.played === 2, "a reused id must chime after release: " + only.played);
}}

// Broken storage never costs the chime.
{{
  const broken = {{ getItem() {{ throw new Error("denied"); }}, setItem() {{ throw new Error("denied"); }} }};
  const t = tab(broken, "running");
  assert(t.chimer.announce("x") === true, "a storage failure must still chime");
  const bad = storage();
  bad.setItem(CHIME_STORAGE_KEY, "not json");
  assert(tab(bad, "running").chimer.announce("y") === true, "junk in storage must still chime");
}}

// The flash starts orange, alternates, and stops cleanly.
{{
  let tick = null;
  let stopped = 0;
  const flasher = new Flasher({{ every: 1000, start: (fn) => {{ tick = fn; return 7; }}, stop: () => {{ stopped += 1; }} }});
  assert(!flasher.loud, "idle is quiet");
  flasher.want(true);
  assert(flasher.loud, "the flash must start on the loud half");
  const look = (loud, watching = false) => tabLook({{ attention: 2, watching, loud, baseTitle: "claude-web" }});
  assert(look(flasher.loud).icon === ICON_FLASH, "the loud half is the orange icon");
  tick();
  assert(!flasher.loud && look(flasher.loud).icon === ICON_ALERT, "the quiet half is the badge");
  flasher.jolt();
  assert(flasher.loud, "a new request jumps back to orange");
  flasher.want(true);
  assert(stopped === 0, "wanting it twice starts one timer");
  flasher.want(false);
  assert(stopped === 1 && !flasher.loud && !flasher.running, "stopping clears the phase");
  flasher.jolt();
  assert(!flasher.loud, "a jolt does not restart a stopped flash");
  assert(look(true, true).icon === ICON_ALERT, "watching, the badge sits still");
  assert(tabLook({{ attention: 0, watching: false, loud: true, baseTitle: "t" }}).icon === ICON, "nothing waiting, no alert");
  assert(tabLook({{ attention: 0, watching: false, loud: true, baseTitle: "t" }}).title === "t", "and the title is restored");
}}

// A finished agent blinks green, but a request for approval wins over it.
{{
  const look = (attention, done, loud, watching = false) => tabLook({{ attention, done, watching, loud, baseTitle: "t" }});
  assert(look(0, 1, true).icon === ICON_DONE_FLASH, "the loud half of a done agent is the green icon");
  assert(look(0, 1, false).icon === ICON, "the quiet half is the plain turtle");
  assert(look(0, 2, true).title === "✅ 2 agents done", "the title counts the done agents: " + look(0, 2, true).title);
  assert(look(1, 3, true).icon === ICON_FLASH, "orange takes precedence over green");
  assert(look(1, 3, false).icon === ICON_ALERT, "even on the quiet half");
  assert(look(1, 3, true).title.startsWith("🔔 1 approval"), "and the title is about the approval");
  assert(look(0, 1, true, true).icon === ICON && look(0, 1, true, true).title === "t", "watching, nothing blinks green");
  assert(ICON_DONE_FLASH !== ICON_FLASH, "the done flash is its own icon");
}}

// Which status changes count as a finished turn.
{{
  assert(finishedTurn("working", "idle"), "working to idle is a finished turn");
  assert(!finishedTurn("awaiting_approval", "idle"), "a turn cut off mid-request was not finished");
  assert(!finishedTurn("starting", "idle"), "starting up is not finishing a turn");
  assert(!finishedTurn("idle", "idle"), "no change is no news");
  assert(!finishedTurn("working", "stopped") && !finishedTurn("working", "failed"), "stopping or failing is not done");
  assert(!finishedTurn("idle", "working"), "starting a turn is not finishing one");
}}

// The done tracker, on hand-fired timers: a finished turn must settle before
// it counts, and its claim is released whenever the agent moves on.
const tracker = () => {{
  const t = {{ timers: new Map(), next: 1, announced: [], released: [], changes: 0, delays: [], quiet: false }};
  t.done = new DoneTracker({{
    quiet: () => t.quiet,
    announce: (id) => t.announced.push(id),
    release: (id) => t.released.push(id),
    onChange: () => {{ t.changes += 1; }},
    start: (fn, ms) => {{ t.delays.push(ms); const id = t.next++; t.timers.set(id, fn); return id; }},
    cancel: (id) => t.timers.delete(id),
  }});
  t.fire = () => {{ const due = [...t.timers.values()]; t.timers.clear(); for (const fn of due) fn(); }};
  return t;
}};

// Settle, then done.
{{
  const t = tracker();
  t.done.status("a", "working", "idle");
  assert(t.announced.length === 0 && !t.done.done.has("a"), "nothing is announced before it settles");
  assert(t.delays[0] === DONE_SETTLE_MS && DONE_SETTLE_MS >= 1000, "it waits the settle time");
  t.fire();
  assert(JSON.stringify(t.announced) === '["a"]' && t.done.done.has("a"), "after settling it is done and chimes");
}}

// A brief idle between queued prompts never counts.
{{
  const t = tracker();
  t.done.status("a", "working", "idle");
  t.done.status("a", "idle", "working");
  t.fire();
  assert(t.announced.length === 0 && t.done.done.size === 0, "idle then working inside the window: no chime, no green");
  assert(JSON.stringify(t.released) === '["a"]', "leaving idle still releases, announced or not");
}}

// Looking keeps the claim; the agent starting again releases it.
{{
  const t = tracker();
  t.done.status("a", "working", "idle");
  t.fire();
  t.done.seen();
  assert(t.done.done.size === 0, "watching clears the green");
  assert(t.released.length === 0, "but keeps the claim, so the same turn cannot chime twice");
  t.done.status("a", "idle", "working");
  assert(JSON.stringify(t.released) === '["a"]', "starting again releases it");
  t.done.status("a", "working", "awaiting_approval");
  t.done.status("a", "awaiting_approval", "idle");
  t.fire();
  assert(t.announced.length === 1, "a turn cut off mid-request does not chime");
}}

// Stopping takes it off the list and frees the claim.
{{
  const t = tracker();
  t.done.status("a", "working", "idle");
  t.fire();
  t.done.status("a", "idle", "stopped");
  assert(t.done.done.size === 0 && t.released.length === 1, "a stopped agent is no longer done");
}}

// While an approval is pending, orange wins: no done chime and no claim, but
// the agent is still done, so the green shows once the approvals clear.
{{
  const t = tracker();
  t.quiet = true;
  t.done.status("a", "working", "idle");
  t.fire();
  assert(t.announced.length === 0, "no done chime while an approval is pending");
  assert(t.done.done.has("a") && t.changes === 1, "but the agent is still done");
  t.done.status("a", "idle", "working");
  assert(JSON.stringify(t.released) === '["a"]', "leaving idle releases even so, which is harmless");
}}

// A removed agent is forgotten, settling or done.
{{
  const t = tracker();
  t.done.status("a", "working", "idle");
  t.done.forget("a");
  t.fire();
  assert(t.announced.length === 0, "forgetting cancels the settle");
  t.done.status("b", "working", "idle");
  t.fire();
  t.done.forget("b");
  assert(!t.done.done.has("b") && JSON.stringify(t.released) === '["a","b"]', "forgetting clears it and frees its claim: " + t.released);
}}

// A snapshot showing the agent not idle frees its claim, even in a tab that
// never saw it finish; an idle one leaves a done agent alone.
{{
  const t = tracker();
  t.done.snapshot("a", "working");
  assert(JSON.stringify(t.released) === '["a"]', "a non-idle snapshot releases: " + t.released);
  t.done.status("b", "working", "idle");
  t.fire();
  t.done.snapshot("b", "idle");
  assert(t.done.done.has("b") && t.released.length === 1, "an idle snapshot keeps the done agent and its claim");
  t.done.status("c", "working", "idle");
  t.done.snapshot("c", "stopped");
  t.fire();
  assert(t.announced.length === 1, "a non-idle snapshot cancels a pending settle");
}}

// Regression: the tab that chimed is gone (navigated away, reloaded), so its
// tracker never releases. A fresh tab sharing the storage sees the agent start
// again, releases the stale claim, and the next finished turn still chimes.
{{
  const shared = storage();
  const doneTab = () => {{
    const t = tab(shared, "running");
    t.timers = [];
    t.done = new DoneTracker({{
      announce: (id) => t.chimer.announce(doneKey(id)),
      release: (id) => t.chimer.claims.release(doneKey(id)),
      start: (fn) => {{ t.timers.push(fn); return t.timers.length; }},
      cancel: () => {{}},
    }});
    t.fire = () => t.timers.splice(0).forEach((fn) => fn());
    return t;
  }};
  const gone = doneTab();
  gone.done.status("x", "working", "idle");
  gone.fire();
  assert(gone.played === 1, "the first tab chimes and claims");
  const fresh = doneTab();
  fresh.done.status("x", "idle", "working");
  fresh.done.status("x", "working", "idle");
  fresh.fire();
  assert(fresh.played === 1, "a tab that never announced still frees the claim, so the next turn chimes");
  // The agent restarts while no tab watches; a later page load sees it working.
  fresh.done.snapshot("x", "working");
  const later = doneTab();
  later.done.status("x", "working", "idle");
  later.fire();
  assert(later.played === 1, "a snapshot frees a claim no live status released");
}}

// The done chime has its own claim: one tab plays it per finished turn, it
// never collides with a request's claim, and releasing it lets the next turn
// chime again.
{{
  const shared = storage();
  const one = tab(shared, "running");
  const two = tab(shared, "running");
  const claims = new ChimeClaims(shared, now);
  assert(doneKey("a") !== attentionKey("a", "done"), "a done claim is not a request claim");
  one.chimer.announce(doneKey("a"));
  two.chimer.announce(doneKey("a"));
  assert(one.played + two.played === 1, "one finished turn, one chime");
  claims.release(doneKey("a"));
  two.chimer.announce(doneKey("a"));
  assert(two.played === 1, "the next finished turn chimes again");
}}

// The dashboard's reconnect reload, modelled with the real Resync and
// PendingRequests: statuses in a Map, chimes counted.
const board = () => {{
  const b = {{ status: new Map(), chimes: [] }};
  b.pending = new PendingRequests((agent, id) => b.chimes.push(agent + ":" + id));
  b.settle = new Map();
  b.nextTimer = 0;
  b.released = [];
  b.done = new DoneTracker({{
    announce: (id) => b.chimes.push("done:" + id),
    release: (id) => b.released.push(id),
    start: (fn) => {{ b.nextTimer += 1; b.settle.set(b.nextTimer, fn); return b.nextTimer; }},
    cancel: (id) => b.settle.delete(id),
  }});
  b.fireSettle = () => {{ const due = [...b.settle.values()]; b.settle.clear(); due.forEach((fn) => fn()); }};
  b.resync = new Resync();
  b.live = (apply) => b.resync.route(apply);
  b.load = (agents, announce = true) => {{
    if (announce) {{
      for (const a of agents) if (b.status.has(a.id)) b.done.status(a.id, b.status.get(a.id), a.status);
      const kept = new Set(agents.map((a) => a.id));
      for (const id of b.status.keys()) if (!kept.has(id)) b.done.forget(id);
    }}
    b.status = new Map(agents.map((a) => [a.id, a.status]));
    b.pending.snapshot(agents, {{ announce }});
  }};
  b.waiting = () => [...b.status.values()].filter((s) => s === "awaiting_approval").length;
  return b;
}};
const agentA = (status, ids) => ({{ id: "a", status, pending_permissions: ids.map((request_id) => ({{ request_id }})) }});

// Stale snapshot: the snapshot still shows r1 waiting, but the socket already
// said it was answered and the agent moved on. The live news must win.
{{
  const b = board();
  b.load([agentA("awaiting_approval", ["r1"])], false);
  const gen = b.resync.begin();
  const snapshot = [agentA("awaiting_approval", ["r1"])];
  b.live(() => b.pending.resolved("a", "r1"));
  b.live(() => b.status.set("a", "working"));
  assert(b.status.get("a") === "awaiting_approval", "live news is held while the reload is in flight");
  assert(b.resync.finish(gen, () => b.load(snapshot)), "the current reload applies");
  assert(b.status.get("a") === "working", "a stale snapshot must not put the agent back");
  assert(b.waiting() === 0 && b.pending.keys.size === 0, "nothing is waiting, so nothing flashes");
  assert(b.chimes.length === 0, "and nothing chimed");
}}

// A request that lands after the snapshot was taken: the snapshot lacks it,
// the held live message restores it, and it chimes exactly once — then not
// again on the next reconnect, whose snapshot holds it.
{{
  const b = board();
  b.load([agentA("working", [])], false);
  const gen = b.resync.begin();
  const snapshot = [agentA("working", [])];
  b.live(() => {{ b.status.set("a", "awaiting_approval"); b.pending.request("a", "r"); }});
  b.resync.finish(gen, () => b.load(snapshot));
  assert(b.pending.keys.has("a:r") && b.waiting() === 1, "the late request must survive the snapshot");
  assert(b.chimes.length === 1, "and chime once: " + b.chimes);
  const again = b.resync.begin();
  b.live(() => b.pending.request("a", "r"));
  b.resync.finish(again, () => b.load([agentA("awaiting_approval", ["r"])]));
  assert(b.chimes.length === 1, "a reconnect must not chime it again: " + b.chimes);
}}

// A request only the snapshot knows about — it landed while the socket was
// down, or before it was first up — chimes; one already known does not.
{{
  const b = board();
  b.load([agentA("awaiting_approval", ["r1"])], false);
  assert(b.chimes.length === 0, "the load at startup stays quiet");
  const gen = b.resync.begin();
  b.resync.finish(gen, () => b.load([agentA("awaiting_approval", ["r1", "r2"])]));
  assert(JSON.stringify(b.chimes) === '["a:r2"]', "only the unseen request chimes: " + b.chimes);
}}

// A turn that ended while the socket was down shows only in the reconnect
// snapshot: it still settles and chimes as done, once.
{{
  const b = board();
  b.load([agentA("working", [])], false);
  const gen = b.resync.begin();
  b.resync.finish(gen, () => b.load([agentA("idle", [])]));
  b.fireSettle();
  assert(JSON.stringify(b.chimes) === '["done:a"]', "the missed finish chimes: " + b.chimes);
  const again = b.resync.begin();
  b.resync.finish(again, () => b.load([agentA("idle", [])]));
  assert(b.settle.size === 0 && b.chimes.length === 1, "a later reconnect does not chime it again");
}}

// Agents removed while the socket was down are missing from the snapshot:
// forgotten, whether already done or still settling.
{{
  const b = board();
  const agentB = (status) => ({{ id: "b", status, pending_permissions: [] }});
  b.load([agentA("working", []), agentB("working")], false);
  b.live(() => {{ b.done.status("a", "working", "idle"); b.status.set("a", "idle"); }});
  b.fireSettle();
  b.live(() => {{ b.done.status("b", "working", "idle"); b.status.set("b", "idle"); }});
  assert(b.done.done.has("a") && b.settle.size === 1, "a is done and b is settling");
  const gen = b.resync.begin();
  b.resync.finish(gen, () => b.load([]));
  assert(b.done.done.size === 0, "nothing removed stays done");
  assert(JSON.stringify(b.released) === '["a","b"]', "both claims are released: " + b.released);
  assert(b.settle.size === 0, "the settling agent's timer is cancelled");
  b.fireSettle();
  assert(JSON.stringify(b.chimes) === '["done:a"]', "and it never chimes: " + b.chimes);
}}

// A snapshot taken before the agent finished says "working", while the finish
// itself is held. Another tab has already settled and claimed it. The release
// pass runs on the statuses after the replay, so the claim stands and this tab
// does not chime a second time.
{{
  const shared = storage();
  const other = tab(shared, "running");
  const here = tab(shared, "running");
  const b = board();
  const settle = [];
  b.done = new DoneTracker({{
    announce: (id) => here.chimer.announce(doneKey(id)),
    release: (id) => here.chimer.claims.release(doneKey(id)),
    start: (fn) => {{ settle.push(fn); return settle.length; }},
    cancel: () => {{}},
  }});
  b.load([agentA("working", [])], false);
  const gen = b.resync.begin();
  b.live(() => {{ b.done.status("a", b.status.get("a"), "idle"); b.status.set("a", "idle"); }});
  other.chimer.announce(doneKey("a"));
  if (b.resync.finish(gen, () => b.load([agentA("working", [])]))) {{
    for (const [id, status] of b.status) b.done.snapshot(id, status);
  }}
  settle.splice(0).forEach((fn) => fn());
  assert(other.played === 1 && here.played === 0, "one finish, one chime across tabs: " + here.played);
  assert(new ChimeClaims(shared, now).read()[doneKey("a")] !== undefined, "the other tab's claim still stands");
}}

// Overlapping reloads: the older result is discarded, and what was held before
// the newer fetch began is covered by its snapshot.
{{
  const b = board();
  b.load([agentA("working", [])], false);
  const first = b.resync.begin();
  b.live(() => b.status.set("a", "stale-live"));
  const second = b.resync.begin();
  b.live(() => b.status.set("a", "newest"));
  assert(b.resync.finish(second, () => b.load([agentA("after-first", [])])), "the newer reload applies");
  assert(b.status.get("a") === "newest", "held news replays on top: " + b.status.get("a"));
  assert(!b.resync.finish(first, () => b.load([agentA("oldest", [])])), "the older reload is discarded");
  assert(b.status.get("a") === "newest", "and changes nothing");
  assert(!b.resync.holding, "nothing is held afterwards");
  b.live(() => b.status.set("a", "direct"));
  assert(b.status.get("a") === "direct", "with no reload in flight, news applies at once");
}}

// A failed reload still applies what it held, onto what the page had.
{{
  const b = board();
  b.load([agentA("working", [])], false);
  const gen = b.resync.begin();
  b.live(() => b.status.set("a", "idle"));
  b.resync.finish(gen, null);
  assert(b.status.get("a") === "idle", "held news must not be lost when the fetch fails");
}}

// A reload that stalls hits its deadline: the fetch is aborted, and the held
// news flushes instead of waiting forever.
{{
  const b = board();
  b.load([agentA("working", [])], false);
  let fire = null;
  let cleared = 0;
  const timers = {{ set: (fn) => {{ fire = fn; return 1; }}, clear: () => {{ cleared += 1; }} }};
  let seen = null;
  const gen = b.resync.begin();
  // A fetch on a half-open connection: never settles, whatever the signal says.
  const reload = withDeadline((signal) => {{ seen = signal; return new Promise(() => {{}}); }}, 10000, timers)
    .then((data) => b.resync.finish(gen, () => b.load(data)), () => b.resync.finish(gen, null));
  b.live(() => b.status.set("a", "awaiting_approval"));
  b.live(() => b.pending.request("a", "r"));
  assert(b.resync.holding && b.status.get("a") === "working", "held while the reload hangs");
  fire();
  await reload;
  assert(seen.aborted, "the stalled fetch must be aborted");
  assert(cleared === 1, "the timer is cleared on settle");
  assert(!b.resync.holding, "the deadline must end the hold");
  assert(b.status.get("a") === "awaiting_approval" && b.chimes.length === 1, "and the held news applies and chimes");
}}

// A reload that answers in time clears its timer and never aborts.
{{
  let fire = null;
  let cleared = 0;
  const timers = {{ set: (fn) => {{ fire = fn; return 1; }}, clear: () => {{ cleared += 1; }} }};
  let seen = null;
  const value = await withDeadline(async (signal) => {{ seen = signal; return 42; }}, 10000, timers);
  assert(value === 42 && cleared === 1 && !seen.aborted, "a prompt reload is untouched");
  const threw = await withDeadline(() => {{ throw new Error("boom"); }}, 10000, timers).then(() => null, (e) => e.message);
  assert(threw === "boom" && cleared === 2, "a synchronous throw still rejects and clears");
}}

// What a resync finds that the socket never announced.
assert(JSON.stringify(newKeys(new Set(["a:1"]), ["a:1", "a:2", "b:1"])) === '["a:2","b:1"]', "newKeys");
console.log("ok");
"#,
            module = module.display()
        );
        std::fs::write(&driver, source).expect("write driver");

        let output = match std::process::Command::new("node").arg(&driver).output() {
            Ok(output) => output,
            // No node installed: nothing in the build depends on it.
            Err(_) => return,
        };
        assert!(
            output.status.success(),
            "attention driver failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A checkbox the panel fills but never sends back is a setting that
    /// silently reverts on every save, so both halves are asserted.
    #[test]
    fn the_settings_panel_reads_and_writes_the_remote_control_toggle() {
        let html = std::str::from_utf8(&Assets::get("index.html").expect("index.html").data)
            .expect("utf-8")
            .to_string();
        let js = std::str::from_utf8(&Assets::get("dashboard.js").expect("dashboard.js").data)
            .expect("utf-8")
            .to_string();
        assert!(html.contains("id=\"cfg-remote-control\""), "no checkbox");
        assert!(
            js.contains("$('cfg-remote-control').checked = cfg.remote_control"),
            "the panel never shows the saved value"
        );
        assert!(
            js.contains("remote_control: $('cfg-remote-control').checked"),
            "the panel never sends the value back"
        );
    }

    /// Same two halves for the auto-resume toggle, and one more: a checkbox
    /// that starts unticked because the panel forgot to fill it turns the
    /// setting *off* on the next save, which is the failure that matters here.
    #[test]
    fn the_settings_panel_reads_and_writes_the_auto_resume_toggle() {
        let html = std::str::from_utf8(&Assets::get("index.html").expect("index.html").data)
            .expect("utf-8")
            .to_string();
        let js = std::str::from_utf8(&Assets::get("dashboard.js").expect("dashboard.js").data)
            .expect("utf-8")
            .to_string();
        assert!(html.contains("id=\"cfg-auto-resume\""), "no checkbox");
        assert!(
            js.contains("$('cfg-auto-resume').checked = cfg.auto_resume"),
            "the panel never shows the saved value"
        );
        assert!(
            js.contains("auto_resume: $('cfg-auto-resume').checked"),
            "the panel never sends the value back"
        );
    }

    /// The text size field is filled, sent back and applied, and the bounds the
    /// browser offers are the bounds the server enforces — in the input and in
    /// the helper that applies it.
    #[test]
    fn the_settings_panel_reads_writes_and_applies_the_text_size() {
        use crate::config::{DEFAULT_TEXT_SIZE, MAX_TEXT_SIZE, MIN_TEXT_SIZE};
        let asset = |name: &str| {
            std::str::from_utf8(&Assets::get(name).expect(name).data)
                .expect("utf-8")
                .to_string()
        };
        let html = asset("index.html");
        let js = asset("dashboard.js");
        let common = asset("common.js");
        let agent = asset("agent.js");
        let css = asset("app.css");
        assert!(
            html.contains(&format!(
                "id=\"cfg-text-size\" type=\"number\" min=\"{MIN_TEXT_SIZE}\" max=\"{MAX_TEXT_SIZE}\""
            )),
            "the input's bounds must match config.rs"
        );
        assert!(
            html.contains(&format!("Text size (px, {MIN_TEXT_SIZE}–{MAX_TEXT_SIZE})")),
            "the label's bounds must match config.rs"
        );
        for (name, value) in [
            ("MIN_TEXT_SIZE", MIN_TEXT_SIZE),
            ("MAX_TEXT_SIZE", MAX_TEXT_SIZE),
            ("DEFAULT_TEXT_SIZE", DEFAULT_TEXT_SIZE),
        ] {
            assert!(
                common.contains(&format!("export const {name} = {value};")),
                "common.js {name} must match config.rs"
            );
        }
        assert!(
            css.contains(&format!("html {{ font-size: {DEFAULT_TEXT_SIZE}px; }}")),
            "the stylesheet's own default must be the configured default"
        );
        assert!(
            js.contains("$('cfg-text-size').value = cfg.text_size"),
            "the panel never shows the saved value"
        );
        assert!(
            js.contains("text_size: $('cfg-text-size').value"),
            "the panel never sends the value back"
        );
        assert!(
            js.contains("applyTextSize(state.config.text_size)"),
            "the dashboard never applies it"
        );
        assert!(
            agent.contains("applyTextSize(cfg.text_size)"),
            "the agent page never applies it"
        );
    }

    #[test]
    fn every_permission_picker_offers_every_mode() {
        let html = std::str::from_utf8(&Assets::get("index.html").expect("index.html").data)
            .expect("utf-8")
            .to_string();
        let agent_html = std::str::from_utf8(&Assets::get("agent.html").expect("agent.html").data)
            .expect("utf-8")
            .to_string();
        for mode in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
            PermissionMode::Dangerous,
        ] {
            // Exhaustive on purpose: a new variant fails to compile here rather
            // than quietly becoming a default the spawn form cannot select.
            match mode {
                PermissionMode::Ask
                | PermissionMode::AcceptEdits
                | PermissionMode::Bypass
                | PermissionMode::Dangerous => {}
            }
            let option = format!("value=\"{}\"", mode.as_str());
            assert!(
                html.matches(&option).count() >= 2,
                "{} is missing from the spawn picker or the settings picker",
                mode.as_str()
            );
            // The agent page picks the mode again, after launch: a mode missing
            // there is one an agent could be put in and never taken out of.
            assert!(
                agent_html.contains(&option),
                "{} is missing from the agent page's permission picker",
                mode.as_str()
            );
        }
    }

    #[test]
    fn behavior_strings_map_to_decisions() {
        assert!(matches!(
            decision_from("allow", None, None),
            Some(PermissionDecision::Allow { .. })
        ));
        assert!(matches!(
            decision_from("deny", Some("no".into()), None),
            Some(PermissionDecision::Deny { .. })
        ));
        assert!(decision_from("maybe", None, None).is_none());
    }

    #[test]
    fn a_note_body_is_trimmed_and_bounded() {
        assert_eq!(
            clean_note("  ship the parser \n").ok().as_deref(),
            Some("ship the parser")
        );
        assert!(clean_note("").is_err());
        assert!(clean_note("   \n\t ").is_err());
        assert!(clean_note(&"x".repeat(MAX_NOTE_BYTES)).is_ok());
        // Measured after trimming, so trailing newlines cannot push a legal
        // body over the edge.
        assert!(clean_note(&format!(" {} ", "x".repeat(MAX_NOTE_BYTES))).is_ok());
        assert!(clean_note(&"x".repeat(MAX_NOTE_BYTES + 1)).is_err());
    }

    #[tokio::test]
    async fn missing_assets_are_a_404_not_a_panic() {
        let response = serve_embedded("nope.js");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_loopback_hosts_are_answered() {
        let policy = loopback_hosts(7717);
        for host in [
            "127.0.0.1:7717",
            "localhost:7717",
            "[::1]:7717",
            " localhost:7717 ",
        ] {
            assert!(policy.host_allowed(Some(host)), "{host} must be allowed");
        }
        for host in [
            "evil.example:7717",
            "attacker.test:7717",
            "127.0.0.1.nip.io:7717",
            "127.0.0.1:9999",
            "localhost",
            "192.168.1.5:7717",
        ] {
            assert!(!policy.host_allowed(Some(host)), "{host} must be refused");
        }
        assert!(!policy.host_allowed(None), "a missing Host is refused");
        assert!(loopback_hosts(80).host_allowed(Some("localhost")));
    }

    /// Off loopback the rebinding defence widens rather than lifting: the
    /// allowlist becomes loopback, the configured bind address and each
    /// configured hostname, on the port actually being served (§12).
    #[test]
    fn a_remote_bind_answers_to_its_own_address_and_names_and_nothing_else() {
        let policy = HostPolicy::new(
            7717,
            "100.64.0.7".parse().expect("ip"),
            &["laptop.tail-scale.ts.net".to_string(), "  ".to_string()],
        );
        for host in [
            // Loopback is still served, and still answered for.
            "127.0.0.1:7717",
            "localhost:7717",
            "[::1]:7717",
            "100.64.0.7:7717",
            "laptop.tail-scale.ts.net:7717",
            "LAPTOP.Tail-Scale.TS.NET:7717",
        ] {
            assert!(policy.host_allowed(Some(host)), "{host} must be allowed");
        }
        for host in [
            // A hostile domain rebound to the tailnet address still arrives
            // carrying its own name, which is on no list.
            "evil.example:7717",
            "laptop.tail-scale.ts.net.evil.example:7717",
            "100.64.0.7:9999",
            "100.64.0.8:7717",
            // An unconfigured name is not a name we answer to.
            "phone.tail-scale.ts.net:7717",
        ] {
            assert!(!policy.host_allowed(Some(host)), "{host} must be refused");
        }
        assert!(policy.origin_allowed(Some("http://100.64.0.7:7717")));
        assert!(policy.origin_allowed(Some("http://laptop.tail-scale.ts.net:7717")));
        assert!(!policy.origin_allowed(Some("http://evil.example:7717")));
    }

    #[test]
    fn only_loopback_origins_are_accepted() {
        let policy = loopback_hosts(7717);
        assert!(
            policy.origin_allowed(None),
            "non-browser clients send no Origin"
        );
        assert!(policy.origin_allowed(Some("http://127.0.0.1:7717")));
        assert!(policy.origin_allowed(Some("http://localhost:7717")));
        for origin in [
            "http://evil.example",
            "https://evil.example:7717",
            "http://localhost:3000",
            "null",
            "file://",
        ] {
            assert!(
                !policy.origin_allowed(Some(origin)),
                "{origin} must be refused"
            );
        }
    }

    /// The loopback-only policy of §7, which is what most of these assert on.
    fn loopback_hosts(port: u16) -> HostPolicy {
        HostPolicy::new(port, IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), &[])
    }

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn test_state() -> AppState {
        test_state_with(None, loopback_hosts(7717))
    }

    fn test_state_with(remote: Option<Arc<RemoteKey>>, hosts: HostPolicy) -> AppState {
        let db = crate::db::Db::open_in_memory().expect("db");
        let config = Arc::new(tokio::sync::RwLock::new(Config::default()));
        AppState {
            sup: Supervisor::new(db, config),
            config_path: PathBuf::from("/dev/null"),
            hosts: Arc::new(hosts),
            token: Arc::new(SessionToken(TEST_TOKEN.to_string())),
            remote,
            refusals: Default::default(),
        }
    }

    /// A paired device key, and the state that accepts it.
    fn paired_state() -> (tempfile::TempDir, String, AppState) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remote-key");
        let key = crate::remote::generate(&path).expect("generate");
        let remote = RemoteKey::load(&path).expect("load").expect("paired");
        let hosts = HostPolicy::new(7717, "100.64.0.7".parse().expect("ip"), &[]);
        (dir, key, test_state_with(Some(Arc::new(remote)), hosts))
    }

    #[test]
    fn refusals_collapse_to_one_line_a_minute_per_path() {
        let log = RefusalLog::default();
        let t0 = Instant::now();

        // The first is always news.
        assert_eq!(log.tally("/ws", t0), Some(0));

        // A page retrying every 16s: 16s, 32s and 48s all land inside the
        // minute and stay quiet.
        for i in 1..=3 {
            assert_eq!(
                log.tally("/ws", t0 + Duration::from_secs(16 * i)),
                None,
                "retry at {}s",
                16 * i
            );
        }

        // The next one is past the window, so it speaks again — and says how
        // many it stands for.
        assert_eq!(
            log.tally("/ws", t0 + Duration::from_secs(64)),
            Some(3),
            "the suppressed ones must be counted, not lost"
        );

        // The count resets, so the next line is not cumulative.
        let t2 = t0 + Duration::from_secs(64);
        assert_eq!(log.tally("/ws", t2 + Duration::from_secs(1)), None);
        assert_eq!(
            log.tally("/ws", t2 + REFUSAL_QUIET + Duration::from_secs(1)),
            Some(1)
        );

        // A different path is throttled on its own clock: a genuine refusal
        // elsewhere is never swallowed by a noisy one.
        assert_eq!(log.tally("/api/agents", t2), Some(0));
    }

    #[test]
    fn the_refusal_table_cannot_grow_without_bound() {
        let log = RefusalLog::default();
        let t0 = Instant::now();
        for i in 0..200 {
            log.tally(&format!("/api/{i}"), t0);
        }
        assert!(
            log.seen.lock().expect("lock").len() <= 64,
            "the map has to stay capped"
        );
    }

    #[tokio::test]
    async fn assets_are_always_revalidated() {
        // Their URLs carry no content hash, so a cached `app.js` from before an
        // upgrade would otherwise keep running.
        for path in ["index.html", "common.js", "app.css"] {
            let response = serve_embedded(path);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("no-cache"),
                "{path} may not be served without a revalidation directive"
            );
        }
    }

    async fn status_of(request: axum::http::Request<axum::body::Body>) -> StatusCode {
        status_from(LOOPBACK_PEER, test_state().await, request).await
    }

    const LOOPBACK_PEER: &str = "127.0.0.1:51234";

    /// Drive one request through the real router as if it had arrived from
    /// `peer`. The server mounts the service with connect info, so the guard
    /// always has this; the tests have to supply it too.
    async fn status_from(
        peer: &str,
        state: AppState,
        mut request: axum::http::Request<axum::body::Body>,
    ) -> StatusCode {
        use tower::ServiceExt;
        let peer: SocketAddr = peer.parse().expect("peer address");
        request.extensions_mut().insert(ConnectInfo(peer));
        router(state)
            .oneshot(request)
            .await
            .expect("response")
            .status()
    }

    /// A request as a browser page of ours would make it.
    fn api_request(path: &str) -> axum::http::request::Builder {
        axum::http::Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7717")
    }

    #[tokio::test]
    async fn the_loopback_guard_lets_a_local_browser_through() {
        let request = api_request("/api/health")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .header("sec-fetch-site", "same-origin")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::OK);
    }

    // -- the token ----------------------------------------------------------

    #[tokio::test]
    async fn an_origin_less_cross_origin_get_is_refused() {
        // `<img src="http://127.0.0.1:7717/api/repos">` on any page: no Origin
        // is sent, and Host is loopback because that is what the URL says. This
        // reached every GET route before the token and the Sec-Fetch-Site check.
        let request = api_request("/api/repos")
            .header("sec-fetch-site", "cross-site")
            .header("sec-fetch-mode", "no-cors")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::FORBIDDEN);

        // And with no Sec-Fetch-Site at all — an older browser, or curl — the
        // token still stands in the way.
        let request = api_request("/api/repos")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::UNAUTHORIZED);
    }

    /// Every route that carries data or changes state, enumerated: one added
    /// later that forgets the check is exactly what this catches.
    const GUARDED_ROUTES: [(&str, &str); 20] = [
        ("GET", "/api/health"),
        ("GET", "/api/repos"),
        ("GET", "/api/agents"),
        ("GET", "/api/rate_limit"),
        ("GET", "/api/config"),
        ("PUT", "/api/config"),
        ("POST", "/api/agents"),
        ("POST", "/api/repos/clone"),
        ("POST", "/api/agents/x/permission_mode"),
        ("POST", "/api/agents/x/stop"),
        ("DELETE", "/api/agents/x"),
        ("GET", "/api/notes"),
        ("POST", "/api/notes"),
        ("PATCH", "/api/notes/x"),
        ("DELETE", "/api/notes/x"),
        ("GET", "/api/agents/x/uploads"),
        ("POST", "/api/agents/x/uploads?name=a.txt"),
        ("GET", "/api/agents/x/uploads/a.txt"),
        ("DELETE", "/api/agents/x/uploads/a.txt"),
        ("GET", "/ws"),
    ];

    #[tokio::test]
    async fn every_api_route_needs_the_token() {
        for (method, path) in GUARDED_ROUTES {
            let request = api_request(path)
                .method(method)
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .expect("request");
            assert_eq!(
                status_of(request).await,
                StatusCode::UNAUTHORIZED,
                "{method} {path} must not be reachable without the token"
            );
        }
    }

    /// The same enumeration with a device paired: having a second credential
    /// must not make "no credential at all" reach anything (§12).
    #[tokio::test]
    async fn every_api_route_needs_a_credential_off_box_too() {
        for (method, path) in GUARDED_ROUTES {
            for (peer, host) in [
                (LOOPBACK_PEER, "127.0.0.1:7717"),
                ("100.64.0.9:41000", "100.64.0.7:7717"),
            ] {
                let (_dir, _key, state) = paired_state();
                let request = axum::http::Request::builder()
                    .uri(path)
                    .method(method)
                    .header("host", host)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .expect("request");
                assert_eq!(
                    status_from(peer, state, request).await,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} must not be reachable from {peer} without a credential"
                );
            }
        }
    }

    /// The per-boot token is delivered by strictly local means, so it never
    /// legitimately arrives from off-box (§12). Refusing it there keeps §7's one
    /// acknowledged leak — a server run as `claude-web > log` — a local problem
    /// rather than a remotely replayable credential.
    #[tokio::test]
    async fn the_per_boot_token_is_refused_when_the_peer_is_not_loopback() {
        let build = || {
            axum::http::Request::builder()
                .uri("/api/health")
                .header("host", "100.64.0.7:7717")
                .header(TOKEN_HEADER, TEST_TOKEN)
                .body(axum::body::Body::empty())
                .expect("request")
        };
        let (_dir, _key, state) = paired_state();
        assert_eq!(
            status_from("100.64.0.9:41000", state, build()).await,
            StatusCode::UNAUTHORIZED,
            "this run's session token may not be replayed from off-box"
        );

        // The same token from the machine itself is the ordinary case.
        let (_dir, _key, state) = paired_state();
        assert_eq!(
            status_from(LOOPBACK_PEER, state, build()).await,
            StatusCode::OK
        );
    }

    /// A paired device may do everything a loopback client may do, from
    /// anywhere the bind allows — including relaxing a permission mode, which
    /// is the reason to want this on a phone at all (§12).
    #[tokio::test]
    async fn a_paired_key_is_accepted_from_any_allowed_peer() {
        for peer in ["100.64.0.9:41000", LOOPBACK_PEER] {
            let (_dir, key, state) = paired_state();
            let request = axum::http::Request::builder()
                .uri("/api/health")
                .header("host", "100.64.0.7:7717")
                .header(TOKEN_HEADER, &key)
                .body(axum::body::Body::empty())
                .expect("request");
            assert_eq!(
                status_from(peer, state, request).await,
                StatusCode::OK,
                "a paired device must be served at {peer}"
            );
        }

        // A key that is not the paired one is nothing, wherever it comes from.
        let (_dir, _key, state) = paired_state();
        let request = axum::http::Request::builder()
            .uri("/api/health")
            .header("host", "100.64.0.7:7717")
            .header(TOKEN_HEADER, "f".repeat(64))
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(
            status_from("100.64.0.9:41000", state, request).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Where this thing listens is not editable through the control plane it
    /// serves: whatever the body claims, the running values are kept (§12).
    #[tokio::test]
    async fn the_settings_panel_cannot_move_the_listening_address() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = test_state().await;
        state.config_path = dir.path().join("config.toml");
        let mut body = serde_json::to_value(Config::default()).expect("config");
        body["bind"] = json!("0.0.0.0");
        body["hostnames"] = json!(["evil.example"]);
        body["max_agents"] = json!(3);
        body["remote_control"] = json!(true);
        body["auto_resume"] = json!(false);
        let mut request = axum::http::Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header("host", "127.0.0.1:7717")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
        ));
        let response = router(state).oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let saved: Config = serde_json::from_slice(&bytes).expect("config");
        assert_eq!(saved.bind, crate::config::DEFAULT_BIND);
        assert!(saved.hostnames.is_empty());
        // The rest of the panel still works, Remote Control included: unlike
        // the bind, it is an ordinary setting the panel owns (§9).
        assert_eq!(saved.max_agents, 3);
        assert!(saved.remote_control);
        assert!(!saved.auto_resume, "and auto-resume can be turned off");
        let on_disk = Config::from_toml_str(
            &std::fs::read_to_string(dir.path().join("config.toml")).expect("read"),
        )
        .expect("parse");
        assert_eq!(on_disk.bind, crate::config::DEFAULT_BIND);
        assert!(on_disk.remote_control, "and it survives the round trip");
    }

    /// A text size outside the bounds is refused with a 400 and nothing is
    /// written; one inside them is saved and served back.
    #[tokio::test]
    async fn the_settings_panel_text_size_is_held_to_its_bounds() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = test_state().await;
        state.config_path = dir.path().join("config.toml");
        let put = |state: AppState, text_size: Value| async move {
            let mut body = serde_json::to_value(Config::default()).expect("config");
            body["text_size"] = text_size;
            let mut request = axum::http::Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("host", "127.0.0.1:7717")
                .header(TOKEN_HEADER, TEST_TOKEN)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request");
            request.extensions_mut().insert(ConnectInfo(
                LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
            ));
            router(state).oneshot(request).await.expect("response")
        };

        for bad in [json!(9), json!(25), json!(0)] {
            let response = put(state.clone(), bad.clone()).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        for unparseable in [json!(-1), json!(300), json!(13.5), json!("big")] {
            let response = put(state.clone(), unparseable.clone()).await;
            assert!(
                response.status().is_client_error(),
                "{unparseable} must be refused"
            );
        }
        assert!(
            !dir.path().join("config.toml").exists(),
            "a refused size is never written"
        );
        assert_eq!(state.sup.config().await.text_size, 13);

        let response = put(state.clone(), json!(18)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.sup.config().await.text_size, 18);
        let on_disk = Config::from_toml_str(
            &std::fs::read_to_string(dir.path().join("config.toml")).expect("read"),
        )
        .expect("parse");
        assert_eq!(on_disk.text_size, 18);
    }

    /// A GET through the whole stack, so the picker's payload is asserted as
    /// the browser receives it rather than as `scan` builds it.
    async fn get_json(state: AppState, path: &str) -> Value {
        use tower::ServiceExt;
        let mut request = api_request(path)
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(axum::body::Body::empty())
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
        ));
        let response = router(state).oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    /// The root folder is offered in the picker as a workspace of its own, so
    /// there is something to click for an agent that spans repositories (§6).
    #[tokio::test]
    async fn the_picker_offers_the_root_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("alpha")).expect("mkdir");
        let state = test_state().await;
        let root = dir.path().to_string_lossy().to_string();
        state
            .sup
            .set_config(Config {
                repo_roots: vec![root.clone()],
                ..Config::default()
            })
            .await;

        let body = get_json(state, "/api/repos").await;
        let roots = body["roots"].as_array().expect("roots group");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0]["path"], json!(root));
        assert_eq!(roots[0]["is_root"], json!(true));
        assert_eq!(roots[0]["is_git"], json!(false));
        // And it does not displace the repositories under it.
        let all = body["all"].as_array().expect("all");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["name"], json!("alpha"));
        assert_eq!(all[0]["is_root"], json!(false));
    }

    /// The branch endpoint answers a root as a root: no branches to choose
    /// from, so the form has nothing to offer that the spawn would ignore.
    #[tokio::test]
    async fn the_branch_endpoint_reports_a_root_as_rootless() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The root is itself a checkout, which must not change the answer. A
        // real one: a bare `.git` directory is enough for `is_git_repo` but not
        // for git, and would exercise the unreadable-config path instead.
        if crate::repo::git::git(dir.path(), &["init", "-q", "-b", "main", "."]).is_err() {
            return;
        }
        let state = test_state().await;
        let root = dir.path().to_string_lossy().to_string();
        state
            .sup
            .set_config(Config {
                repo_roots: vec![root.clone()],
                ..Config::default()
            })
            .await;

        let body = get_json(state, &format!("/api/repos/branches?path={root}")).await;
        assert_eq!(body["is_root"], json!(true));
        assert_eq!(body["is_git"], json!(false));
        assert_eq!(body["dirty"], json!(false));
        assert_eq!(body["current"], Value::Null);
        assert_eq!(body["branches"], json!([]));

        // And the picker agrees: a real repository as a root declares nothing
        // that runs, so it is offered rather than badged "not inspected".
        let listing = crate::repo::scan::scan_roots(
            &[dir.path().to_path_buf()],
            &std::collections::HashMap::new(),
        );
        assert_eq!(listing.roots.len(), 1);
        assert_eq!(listing.roots[0].refused, None);
    }

    #[tokio::test]
    async fn the_detail_payload_carries_the_composer_history() {
        use tower::ServiceExt;
        let state = test_state().await;
        let record = {
            let db = state.sup.db().clone();
            db.run(|db| {
                let record = crate::db::AgentRecord {
                    id: "agent-1".to_string(),
                    name: "Fix the parser".to_string(),
                    slug: "fix_the_parser".to_string(),
                    repo_path: "/repos/thing".to_string(),
                    work_path: "/repos/thing".to_string(),
                    is_git: false,
                    branch: None,
                    base_ref: None,
                    uses_worktree: false,
                    branch_is_new: false,
                    is_root: false,
                    permission_mode: crate::agent::state::PermissionMode::Ask,
                    model: None,
                    effort: None,
                    max_budget_usd: None,
                    add_dirs: Vec::new(),
                    status: crate::agent::state::Status::Stopped,
                    status_detail: None,
                    exit_code: None,
                    last_stderr: None,
                    cost_usd: 0.0,
                    created_at: 1,
                    last_active_at: 1,
                };
                db.insert_agent(&record)?;
                for text in ["run the tests", "now fix the failure"] {
                    db.append_event(
                        &record.id,
                        crate::agent::protocol::EventKind::User,
                        &json!({"type": "user", "message": {"role": "user", "content": text}}),
                    )?;
                }
                Ok(record)
            })
            .await
            .expect("seed")
        };

        let mut request = api_request(&format!("/api/agents/{}", record.slug))
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(axum::body::Body::empty())
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
        ));
        let response = router(state).oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        // Oldest first, which is the order the composer walks backwards through.
        assert_eq!(
            body["input_history"],
            json!(["run the tests", "now fix the failure"])
        );
    }

    // -- uploads ------------------------------------------------------------

    /// A state whose upload folders live in `dir`, with one stopped agent.
    async fn upload_state(dir: &std::path::Path, upload_max_mb: u64) -> AppState {
        let db = crate::db::Db::open_in_memory().expect("db");
        let config = Arc::new(tokio::sync::RwLock::new(Config {
            upload_max_mb,
            ..Config::default()
        }));
        let state = AppState {
            sup: Supervisor::with_uploads_root(db, config, dir.to_path_buf()),
            ..test_state().await
        };
        state
            .sup
            .db()
            .run(|db| {
                db.insert_agent(&crate::db::AgentRecord {
                    id: "agent-1".to_string(),
                    name: "Look at files".to_string(),
                    slug: "look_at_files".to_string(),
                    repo_path: "/repos/thing".to_string(),
                    work_path: "/repos/thing".to_string(),
                    is_git: false,
                    branch: None,
                    base_ref: None,
                    uses_worktree: false,
                    branch_is_new: false,
                    is_root: false,
                    permission_mode: PermissionMode::Ask,
                    model: None,
                    effort: None,
                    max_budget_usd: None,
                    add_dirs: Vec::new(),
                    status: crate::agent::state::Status::Stopped,
                    status_detail: None,
                    exit_code: None,
                    last_stderr: None,
                    cost_usd: 0.0,
                    created_at: 1,
                    last_active_at: 1,
                })
            })
            .await
            .expect("seed");
        state
    }

    /// One authenticated request through the real router.
    async fn call(state: &AppState, method: &str, path: &str, body: Vec<u8>) -> Response {
        use tower::ServiceExt;
        let mut request = api_request(path)
            .method(method)
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(axum::body::Body::from(body))
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
        ));
        router(state.clone())
            .oneshot(request)
            .await
            .expect("response")
    }

    async fn body_of(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("body")
            .to_vec()
    }

    /// Upload, list, download and withdraw, end to end — including a body
    /// over axum's 2 MB default, which only this route may take.
    #[tokio::test]
    async fn an_upload_round_trips_through_the_api() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 50).await;
        let big = vec![7u8; 3 * 1024 * 1024];

        // A stopped agent still takes uploads: they wait for Resume.
        let response = call(
            &state,
            "POST",
            "/api/agents/look_at_files/uploads?name=..%2Fmy%20shot.png",
            big.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let uploaded: Value = serde_json::from_slice(&body_of(response).await).expect("json");
        assert_eq!(uploaded["name"], json!("my-shot.png"));
        assert_eq!(uploaded["size"], json!(big.len()));
        let on_disk = dir.path().join("agent-1").join("my-shot.png");
        assert_eq!(uploaded["path"], json!(on_disk.to_string_lossy()));
        assert_eq!(std::fs::read(&on_disk).expect("read"), big);

        // The same name again is numbered, not overwritten.
        let response = call(
            &state,
            "POST",
            "/api/agents/agent-1/uploads?name=my%20shot.png",
            b"second".to_vec(),
        )
        .await;
        let second: Value = serde_json::from_slice(&body_of(response).await).expect("json");
        assert_eq!(second["name"], json!("my-shot-2.png"));

        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads?pending=1",
            vec![],
        )
        .await;
        let listed: Value = serde_json::from_slice(&body_of(response).await).expect("json");
        let names: Vec<&str> = listed["uploads"]
            .as_array()
            .expect("list")
            .iter()
            .map(|u| u["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["my-shot.png", "my-shot-2.png"]);

        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads/my-shot-2.png",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert_eq!(
            headers
                .get(header::CONTENT_DISPOSITION)
                .expect("disposition"),
            "attachment; filename=\"my-shot-2.png\"; filename*=UTF-8''my-shot-2.png"
        );
        assert_eq!(
            headers
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .expect("nosniff"),
            "nosniff"
        );
        assert_eq!(
            headers.get(header::CONTENT_TYPE).expect("type"),
            "application/octet-stream"
        );
        assert_eq!(headers.get(header::CONTENT_LENGTH).expect("length"), "6");
        assert_eq!(body_of(response).await, b"second");

        let response = call(
            &state,
            "DELETE",
            "/api/agents/agent-1/uploads/my-shot-2.png",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!dir.path().join("agent-1").join("my-shot-2.png").exists());
        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads/my-shot-2.png",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // A sent upload is part of the transcript and cannot be withdrawn.
        state
            .sup
            .db()
            .run(|db| db.claim_uploads("agent-1", &["my-shot.png".to_string()]))
            .await
            .expect("mark");
        let response = call(
            &state,
            "DELETE",
            "/api/agents/agent-1/uploads/my-shot.png",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(on_disk.exists());
        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads?pending=1",
            vec![],
        )
        .await;
        let listed: Value = serde_json::from_slice(&body_of(response).await).expect("json");
        assert_eq!(listed["uploads"], json!([]));
        // Sent files still download from the transcript.
        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads/my-shot.png",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A name whose cleaned stem ends in `-` still gets a numbered sibling
    /// that can be downloaded and withdrawn.
    #[tokio::test]
    async fn a_numbered_upload_of_an_awkward_name_can_be_fetched_and_withdrawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 50).await;
        let mut names = Vec::new();
        for body in [b"first".to_vec(), b"second".to_vec()] {
            let response = call(
                &state,
                "POST",
                "/api/agents/agent-1/uploads?name=a%20(1).pdf",
                body,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let uploaded: Value = serde_json::from_slice(&body_of(response).await).expect("json");
            names.push(uploaded["name"].as_str().expect("name").to_string());
        }
        assert_eq!(names, vec!["a-1-.pdf", "a-1-2.pdf"]);

        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads/a-1-2.pdf",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, b"second");
        let response = call(
            &state,
            "DELETE",
            "/api/agents/agent-1/uploads/a-1-2.pdf",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!dir.path().join("agent-1").join("a-1-2.pdf").exists());
        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads?pending=1",
            vec![],
        )
        .await;
        let listed: Value = serde_json::from_slice(&body_of(response).await).expect("json");
        assert_eq!(listed["uploads"].as_array().expect("list").len(), 1);
    }

    /// The download is streamed with the length the file had when it was
    /// opened, so a large one is not read whole into memory.
    #[tokio::test]
    async fn a_large_download_is_streamed_with_its_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 50).await;
        let response = call(
            &state,
            "POST",
            "/api/agents/agent-1/uploads?name=big.bin",
            b"x".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // The agent grows it in place, past anything the upload would allow.
        let big: Vec<u8> = (0..5 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("agent-1").join("big.bin"), &big).expect("grow");

        let response = call(&state, "GET", "/api/agents/agent-1/uploads/big.bin", vec![]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_LENGTH)
                .expect("length"),
            big.len().to_string().as_str()
        );
        assert_eq!(body_of(response).await, big);
    }

    /// The agent may move a sent file away, freeing its name on disk. A new
    /// upload of the same name must not take over the old row: the sent
    /// message's chip and trailer would then point at different content.
    #[tokio::test]
    async fn a_name_once_recorded_is_never_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 50).await;
        let folder = dir.path().join("agent-1");
        let upload = |body: &'static [u8]| {
            let state = state.clone();
            async move {
                let response = call(
                    &state,
                    "POST",
                    "/api/agents/agent-1/uploads?name=image.png",
                    body.to_vec(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let json: Value = serde_json::from_slice(&body_of(response).await).expect("json");
                json["name"].as_str().expect("name").to_string()
            }
        };

        assert_eq!(upload(b"old").await, "image.png");
        state
            .sup
            .db()
            .run(|db| db.claim_uploads("agent-1", &["image.png".to_string()]))
            .await
            .expect("send");
        std::fs::remove_file(folder.join("image.png")).expect("the agent moves it");

        assert_eq!(
            upload(b"new").await,
            "image-2.png",
            "the old name stays taken"
        );
        let old = state
            .sup
            .db()
            .run(|db| db.get_upload("agent-1", "image.png"))
            .await
            .expect("get")
            .expect("old row");
        assert!(old.sent_at.is_some(), "the sent row is untouched");
        assert_eq!(old.size, 3);
        assert!(
            !folder.join("image.png").exists(),
            "nothing was written under the old name"
        );
        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads/image.png",
            vec![],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "the old chip never serves new content"
        );

        // A pending row whose file went, too: the next free name is used, and
        // the move past a taken name leaves no stray placeholder behind.
        std::fs::remove_file(folder.join("image-2.png")).expect("gone");
        assert_eq!(upload(b"third").await, "image-3.png");
        let mut on_disk: Vec<String> = std::fs::read_dir(&folder)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        on_disk.sort();
        assert_eq!(on_disk, vec!["image-3.png"]);
        assert_eq!(
            std::fs::read(folder.join("image-3.png")).expect("read"),
            b"third"
        );
    }

    /// A client that goes away mid-upload — the chip's ×, a dropped
    /// connection — drops the handler at an `.await`. The half-written file
    /// must go with it, not linger with no row holding its name.
    #[tokio::test]
    async fn an_abandoned_upload_leaves_no_file_behind() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 50).await;

        // One chunk, then a signal that the handler is waiting on the next,
        // then silence for ever.
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel::<()>();
        let body =
            futures_util::stream::unfold((0, Some(reached_tx)), |(step, mut reached)| async move {
                if step == 0 {
                    return Some((Ok::<_, std::io::Error>(vec![1u8; 1024]), (1, reached)));
                }
                if let Some(tx) = reached.take() {
                    tx.send(()).ok();
                }
                futures_util::future::pending::<()>().await;
                None
            });
        let mut request = api_request("/api/agents/agent-1/uploads?name=half.bin")
            .method("POST")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(axum::body::Body::from_stream(body))
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
        ));
        let task = tokio::spawn(router(state.clone()).oneshot(request));
        reached_rx.await.expect("the handler reads the body");
        let file = dir.path().join("agent-1").join("half.bin");
        assert!(file.exists(), "the upload is under way");

        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());
        assert!(!file.exists(), "an abandoned upload is removed");
        let totals = state
            .sup
            .db()
            .run(|db| db.upload_totals("agent-1"))
            .await
            .expect("totals");
        assert_eq!(totals, (0, 0));
    }

    /// Over the cap is refused, whether the client says so up front or not,
    /// and leaves nothing on disk or in the table.
    #[tokio::test]
    async fn an_upload_over_the_cap_is_refused_and_leaves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 1).await;
        let response = call(
            &state,
            "POST",
            "/api/agents/agent-1/uploads?name=big.bin",
            vec![0u8; 1024 * 1024 + 1],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // A streamed body carries no length, so the cap is enforced as it
        // arrives.
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            vec![Ok(vec![0u8; 700 * 1024]), Ok(vec![0u8; 700 * 1024])];
        let body = axum::body::Body::from_stream(futures_util::stream::iter(chunks));
        let mut request = api_request("/api/agents/agent-1/uploads?name=streamed.bin")
            .method("POST")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(body)
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            LOOPBACK_PEER.parse::<SocketAddr>().expect("peer"),
        ));
        let response = {
            use tower::ServiceExt;
            router(state.clone())
                .oneshot(request)
                .await
                .expect("response")
        };
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let folder = dir.path().join("agent-1");
        let left: Vec<_> = std::fs::read_dir(&folder)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "partial files must be removed: {left:?}");
        let totals = state
            .sup
            .db()
            .run(|db| db.upload_totals("agent-1"))
            .await
            .expect("totals");
        assert_eq!(totals, (0, 0));

        // Under it is fine.
        let response = call(
            &state,
            "POST",
            "/api/agents/agent-1/uploads?name=ok.bin",
            vec![0u8; 1024],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The agent can write in its own upload folder. A link it plants in
    /// place of an upload must not turn the download route into a way to read
    /// anything else; and a name that was never stored never reaches a path.
    #[tokio::test]
    async fn a_download_refuses_symlinks_and_unstored_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = upload_state(dir.path(), 50).await;
        let response = call(
            &state,
            "POST",
            "/api/agents/agent-1/uploads?name=notes.txt",
            b"mine".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "do not serve").expect("write");
        let planted = dir.path().join("agent-1").join("notes.txt");
        std::fs::remove_file(&planted).expect("rm");
        std::os::unix::fs::symlink(&secret, &planted).expect("symlink");

        let response = call(
            &state,
            "GET",
            "/api/agents/agent-1/uploads/notes.txt",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!String::from_utf8_lossy(&body_of(response).await).contains("do not serve"));

        for path in [
            "/api/agents/agent-1/uploads/..%2Fsecret.txt",
            "/api/agents/agent-1/uploads/.hidden",
            "/api/agents/agent-1/uploads/never-uploaded.txt",
        ] {
            let response = call(&state, "GET", path, vec![]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        // Withdrawing removes the link, never what it points at.
        let response = call(
            &state,
            "DELETE",
            "/api/agents/agent-1/uploads/notes.txt",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read_to_string(&secret).expect("read"),
            "do not serve"
        );
    }

    /// Drive the real `uploads.js` — the composer's attachment decisions — and
    /// hold its size formatting to the Rust one the trailer uses.
    #[test]
    fn the_composer_waits_for_uploads_and_sends_names_only() {
        let module = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets/uploads.js");
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = dir.path().join("uploads.mjs");
        let sizes: Vec<String> = [0u64, 1023, 1024, 1536, 5 * 1024 * 1024, 3 << 30]
            .iter()
            .map(|&n| {
                format!(
                    "assert(humanSize({n}) === {:?}, \"humanSize({n}): \" + humanSize({n}));",
                    crate::uploads::human_size(n)
                )
            })
            .collect();
        let source = format!(
            r#"
import {{ composerState, confirmSent, humanSize, markSending, mergePending, pastedFiles, rejectSending, uploadsUrl }} from "{module}";
const assert = (cond, msg) => {{ if (!cond) {{ console.error("FAIL: " + msg); process.exit(1); }} }};

{sizes}

const chip = (status, name) => ({{ status, name }});

// An upload in flight holds Send back; finished ones go as names.
let s = composerState({{ running: true, text: "hi", chips: [chip("done", "a.png"), chip("uploading", null)] }});
assert(s.blocked && s.reason.includes("uploads"), "an upload in flight blocks Send");
s = composerState({{ running: true, text: "", chips: [chip("done", "a.png"), chip("failed", null)] }});
assert(!s.blocked, "finished uploads do not block");
assert(JSON.stringify(s.attachments) === '["a.png"]', "only finished uploads are attached: " + s.attachments);
assert(!s.empty, "attachments alone are something to send");
s = composerState({{ running: true, text: "  ", chips: [] }});
assert(s.empty, "blank text and no files is nothing");

// A stopped agent keeps its chips, and Send stays grey as it always did.
s = composerState({{ running: false, text: "hi", chips: [chip("done", "a.png")] }});
assert(s.blocked && s.reason.includes("Resume"), "a stopped agent cannot be sent to");

// Pastes: a screenshot or a copied file uploads; rich text does not.
const png = {{ name: "image.png" }};
assert(pastedFiles(["Files"], [png]).length === 1, "a screenshot uploads");
assert(pastedFiles(["text/plain", "Files"], [png]).length === 1, "a copied file uploads");
assert(pastedFiles(["text/plain", "text/html", "Files"], [png]).length === 0, "rich text pastes as text");
assert(pastedFiles(["text/plain"], []).length === 0, "plain text pastes as text");

// After Send, chips wait as `sending` until the server answers.
{{
  const inFlight = chip("uploading", null);
  let chips = markSending([chip("done", "a.png"), chip("done", "b.txt"), inFlight]);
  assert(chips.filter((c) => c.status === "sending").length === 2, "finished chips become sending");
  assert(chips[2] === inFlight, "an upload in flight is left alone, same object");
  s = composerState({{ running: true, text: "", chips }});
  assert(s.attachments.length === 0, "a sending chip is never attached twice");
  assert(!s.blocked || chips.some((c) => c.status === "uploading"), "sending alone does not block");

  // Confirmation removes exactly what the event carried.
  const confirmed = confirmSent(chips, ["a.png"]);
  assert(confirmed.map((c) => c.name).join() === "b.txt,", "only the confirmed chip goes: " + confirmed.map((c) => c.name));

  // A refusal drops the sending chips and the server's pending list brings
  // back whichever were not sent.
  chips = rejectSending(confirmed);
  assert(chips.length === 1 && chips[0] === inFlight, "sending chips are dropped on refusal");
  chips = mergePending(chips, [{{ name: "b.txt", size: 3 }}]);
  assert(chips.length === 2 && chips[1].name === "b.txt" && chips[1].status === "done", "pending ones come back");
  chips = mergePending(chips, [{{ name: "b.txt", size: 3 }}]);
  assert(chips.length === 2, "a chip already shown is not added twice");
}}

assert(uploadsUrl("a b") === "/api/agents/a%20b/uploads", uploadsUrl("a b"));
assert(uploadsUrl("x", "r?é.png") === "/api/agents/x/uploads/r%3F%C3%A9.png", uploadsUrl("x", "r?é.png"));
console.log("ok");
"#,
            module = module.display(),
            sizes = sizes.join("\n"),
        );
        std::fs::write(&driver, source).expect("write driver");
        let output = match std::process::Command::new("node").arg(&driver).output() {
            Ok(output) => output,
            // No node installed: nothing in the build depends on it.
            Err(_) => return,
        };
        assert!(
            output.status.success(),
            "uploads driver failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The page attaches by name over the socket, and renders a sent
    /// message's attachments rather than the trailer the agent read.
    #[test]
    fn the_agent_page_sends_attachment_names_and_renders_chips() {
        let js = std::str::from_utf8(&Assets::get("agent.js").expect("agent.js").data)
            .expect("utf-8")
            .to_string();
        let html = std::str::from_utf8(&Assets::get("agent.html").expect("agent.html").data)
            .expect("utf-8")
            .to_string();
        assert!(
            js.contains(
                "socket.send({ type: 'send_message', agent_id: state.agent.id, text, attachments })"
            ),
            "the message must carry the attachment names"
        );
        assert!(js.contains("p.attachments"), "sent attachments must render");
        assert!(
            js.contains("xhr.upload.onprogress"),
            "uploads show progress"
        );
        // Every delete mentions the uploads it takes, not only a forced one.
        let dashboard =
            std::str::from_utf8(&Assets::get("dashboard.js").expect("dashboard.js").data)
                .expect("utf-8")
                .to_string();
        let preview = dashboard
            .find("/delete_preview")
            .expect("the dashboard asks for the delete preview");
        let first_confirm = dashboard
            .find("confirm(`Delete \"${agent.name}\"?")
            .expect("the delete confirmation");
        assert!(
            preview < first_confirm,
            "the preview comes before the first confirm"
        );
        assert!(dashboard.contains("report.uploads"));
        for id in [
            "id=\"attach\"",
            "id=\"file-input\" type=\"file\" multiple",
            "id=\"uploads\"",
        ] {
            assert!(html.contains(id), "agent.html is missing {id}");
        }
    }

    /// "Open the link claude-web printed when it started" is useless advice on
    /// a phone that has never been near the terminal, so the refusal branches
    /// on the peer the server already knows.
    #[tokio::test]
    async fn a_refusal_says_what_to_do_where_the_client_is() {
        use tower::ServiceExt;
        let body_of = |peer: &'static str| async move {
            let (_dir, _key, state) = paired_state();
            let peer: SocketAddr = peer.parse().expect("peer");
            let mut request = axum::http::Request::builder()
                .uri("/api/health")
                .header("host", "100.64.0.7:7717")
                .body(axum::body::Body::empty())
                .expect("request");
            request.extensions_mut().insert(ConnectInfo(peer));
            let response = router(state).oneshot(request).await.expect("response");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("body");
            String::from_utf8_lossy(&bytes).to_string()
        };
        assert!(body_of(LOOPBACK_PEER).await.contains("printed at startup"));
        let remote = body_of("100.64.0.9:41000").await;
        assert!(remote.contains("claude-web pair"), "{remote}");
        assert!(
            !remote.contains("printed at startup"),
            "a phone cannot see the terminal: {remote}"
        );
    }

    /// A paired device presenting a key that has just been replaced is refused,
    /// and the device paired a moment ago is served without a restart.
    #[tokio::test]
    async fn a_fresh_pairing_needs_no_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remote-key");
        let old = crate::remote::generate(&path).expect("generate");
        let remote = Arc::new(RemoteKey::load(&path).expect("load").expect("paired"));
        let hosts = || HostPolicy::new(7717, "100.64.0.7".parse().expect("ip"), &[]);
        let request = |key: &str| {
            axum::http::Request::builder()
                .uri("/api/health")
                .header("host", "100.64.0.7:7717")
                .header(TOKEN_HEADER, key)
                .body(axum::body::Body::empty())
                .expect("request")
        };

        // `pair` runs again on the machine while the server keeps serving.
        let new = crate::remote::generate(&path).expect("re-pair");
        let state = test_state_with(Some(remote.clone()), hosts());
        assert_eq!(
            status_from("100.64.0.9:41000", state, request(&new)).await,
            StatusCode::OK,
            "the new key must work on the paired device's first request"
        );
        let state = test_state_with(Some(remote), hosts());
        assert_eq!(
            status_from("100.64.0.9:41000", state, request(&old)).await,
            StatusCode::UNAUTHORIZED,
            "the old key is dead immediately, on every device"
        );
    }

    #[tokio::test]
    async fn a_wrong_token_is_no_better_than_none() {
        let request = api_request("/api/agents")
            .header(TOKEN_HEADER, "f".repeat(64))
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::UNAUTHORIZED);

        // Nor is a prefix of the real one.
        let request = api_request("/api/agents")
            .header(TOKEN_HEADER, &TEST_TOKEN[..32])
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_socket_upgrade_needs_the_token_too() {
        let build = |query: &str| {
            axum::http::Request::builder()
                .uri(format!("/ws{query}"))
                .header("host", "127.0.0.1:7717")
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(axum::body::Body::empty())
                .expect("request")
        };
        assert_eq!(status_of(build("")).await, StatusCode::UNAUTHORIZED);
        // A browser cannot set headers on an upgrade, so the token may ride in
        // the query string there.
        assert_ne!(
            status_of(build(&format!("?token={TEST_TOKEN}"))).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn the_pages_stay_navigable_without_a_token() {
        for path in ["/", "/agent/some_slug", "/assets/app.css"] {
            let request = api_request(path)
                .body(axum::body::Body::empty())
                .expect("request");
            assert_eq!(status_of(request).await, StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn served_pages_carry_a_framing_and_content_policy() {
        use tower::ServiceExt;
        let request = api_request("/")
            .body(axum::body::Body::empty())
            .expect("request");
        let response = router(test_state().await)
            .oneshot(request)
            .await
            .expect("response");
        let headers = response.headers();
        assert_eq!(
            headers.get("x-frame-options").map(|v| v.as_bytes()),
            Some(&b"DENY"[..])
        );
        let csp = headers
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert_eq!(
            headers.get("x-content-type-options").map(|v| v.as_bytes()),
            Some(&b"nosniff"[..])
        );
    }

    #[test]
    fn token_comparison_rejects_length_and_content_mismatches() {
        let token = SessionToken(TEST_TOKEN.to_string());
        assert!(token.matches(TEST_TOKEN));
        assert!(!token.matches(""));
        assert!(!token.matches(&TEST_TOKEN[..63]));
        assert!(!token.matches(&format!("{TEST_TOKEN}x")));
        assert!(!token.matches(&"0".repeat(64)));
        // Minted tokens are long, random and never printed by Debug.
        let minted = SessionToken::mint();
        assert_eq!(minted.as_str().len(), 64);
        assert_ne!(minted.as_str(), SessionToken::mint().as_str());
        assert_eq!(format!("{minted:?}"), "SessionToken(<redacted>)");
    }

    #[test]
    fn only_same_origin_fetches_are_allowed() {
        assert!(fetch_site_allowed(None));
        assert!(fetch_site_allowed(Some("same-origin")));
        assert!(fetch_site_allowed(Some("none")));
        assert!(!fetch_site_allowed(Some("cross-site")));
        assert!(!fetch_site_allowed(Some("same-site")));
    }

    #[test]
    fn only_data_and_control_routes_need_the_token() {
        assert!(requires_token("/api/agents"));
        assert!(requires_token("/api/health"));
        assert!(requires_token("/ws"));
        assert!(!requires_token("/"));
        assert!(!requires_token("/agent/slug"));
        assert!(!requires_token("/assets/app.css"));
    }

    #[tokio::test]
    async fn the_loopback_guard_refuses_a_rebound_host() {
        let request = axum::http::Request::builder()
            .uri("/api/health")
            .header("host", "evil.example:7717")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::FORBIDDEN);

        // Even with a loopback Host, a foreign Origin is refused.
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("host", "127.0.0.1:7717")
            .header("origin", "http://evil.example")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_cross_origin_websocket_upgrade_is_refused() {
        let build = |origin: &str| {
            axum::http::Request::builder()
                .uri(format!("/ws?token={TEST_TOKEN}"))
                .header("host", "127.0.0.1:7717")
                .header("origin", origin)
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(axum::body::Body::empty())
                .expect("request")
        };
        assert_eq!(
            status_of(build("http://evil.example")).await,
            StatusCode::FORBIDDEN
        );
        // The same upgrade from the portal itself gets past the guard.
        assert_ne!(
            status_of(build("http://127.0.0.1:7717")).await,
            StatusCode::FORBIDDEN
        );
    }

    /// Drive the real `transcript.js` through the interleaving that broke the
    /// catch-up walk: a live event arriving between replay pages.
    ///
    /// There is no JS test harness in this repo (no build step, by design), so
    /// the walk's cursor and termination logic was factored into
    /// `assets/transcript.js` — free of the DOM and the socket — and is
    /// exercised here through `node`. The test skips itself when `node` is not
    /// installed; it is not needed to build or run the server.
    #[test]
    fn the_catch_up_walk_leaves_no_hole_when_live_events_interleave() {
        let module =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets/transcript.js");
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = dir.path().join("walk.mjs");
        let source = format!(
            r#"
import {{ Transcript, nextWalkCursor }} from "{module}";

const HEAD = 3000;
const PAGE = 500;
const assert = (cond, msg) => {{ if (!cond) {{ console.error("FAIL: " + msg); process.exit(1); }} }};

// A stand-in server holding events 1..HEAD, paginated exactly like ws::replay.
const page = (after) => {{
  const events = [];
  for (let seq = after + 1; seq <= Math.min(after + PAGE, HEAD); seq += 1) {{
    events.push({{ seq, kind: "assistant", payload: {{}} }});
  }}
  const cursor = events.length ? events[events.length - 1].seq : after;
  return {{ after, cursor, events, has_more: cursor < HEAD }};
}};

const transcript = new Transcript();
const rendered = [];
const render = (events) => {{ for (const e of transcript.accept(events)) rendered.push(e.seq); }};

// Reconnect from an old cursor while the agent is still working.
let cursor = 100;
transcript.seed(cursor);
let pages = 0;
let liveInjected = false;
for (;;) {{
  const reply = page(cursor);
  pages += 1;
  render(reply.events);

  // The bus delivers a live event that outruns the page cursor, exactly as the
  // reviewer described. It must not end the walk.
  if (!liveInjected) {{
    liveInjected = true;
    render([{{ seq: 3001, kind: "assistant", payload: {{}} }}]);
    assert(transcript.max === 3001, "the live event should be recorded");
    assert(transcript.replayFrom === 600, "a gap must hold the reconnect cursor back");
  }}

  const next = nextWalkCursor(reply);
  if (next === null) break;
  cursor = next;
  assert(pages < 50, "the walk must terminate");
}}

// Every event between the old cursor and the head arrived, exactly once.
const expected = [];
for (let seq = 101; seq <= HEAD; seq += 1) expected.push(seq);
expected.push(3001);
const sorted = [...rendered].sort((a, b) => a - b);
assert(new Set(rendered).size === rendered.length, "no event may render twice");
assert(
  JSON.stringify(sorted) === JSON.stringify(expected),
  "a hole was left in the transcript: got " + sorted.length + " of " + expected.length
);
assert(transcript.replayFrom === 3001, "the cursor should catch up: " + transcript.replayFrom);
assert(!transcript.hasGap, "no gap should remain");

// Replay and the live stream overlapping is not a double render.
const before = rendered.length;
render([{{ seq: 3001, kind: "assistant", payload: {{}} }}]);
assert(rendered.length === before, "a duplicate seq must be dropped");

// A fresh view starts at the tail and does not chase what it never asked for.
const fresh = new Transcript();
fresh.seed(2500);
render([]);
assert(fresh.replayFrom === 2500, "a fresh view must not walk back to zero");
fresh.accept([{{ seq: 2501, kind: "system", payload: {{}} }}]);
assert(fresh.replayFrom === 2501, "and advances from there");

// An empty page never loops, whatever the server claims.
assert(nextWalkCursor({{ has_more: true, events: [], cursor: 10 }}) === null, "empty page must stop");
assert(nextWalkCursor({{ has_more: false, events: [{{ seq: 1 }}], cursor: 1 }}) === null, "done means done");
console.log("ok");
"#,
            module = module.display()
        );
        std::fs::write(&driver, source).expect("write driver");

        let output = match std::process::Command::new("node").arg(&driver).output() {
            Ok(output) => output,
            // No node installed: the module is still covered by the server-side
            // has_more tests, and nothing in the build depends on node.
            Err(_) => return,
        };
        assert!(
            output.status.success(),
            "transcript walk failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Drive the real `splitter.js` — the divider's bounds, keys and storage,
    /// kept free of the DOM for exactly this — and hold the stylesheet's
    /// fallback and backstop to the numbers it uses.
    #[test]
    fn the_side_panel_divider_clamps_steps_and_remembers_its_width() {
        let css = std::str::from_utf8(&Assets::get("app.css").expect("app.css").data)
            .expect("utf-8")
            .to_string();
        let html = std::str::from_utf8(&Assets::get("agent.html").expect("agent.html").data)
            .expect("utf-8")
            .to_string();
        assert!(
            css.contains("clamp(300px, var(--side-width, 480px), calc(100% - 496px))"),
            "the stylesheet's rail track must match splitter.js"
        );
        assert!(
            html.contains("id=\"splitter\" role=\"separator\""),
            "the agent page must carry the divider"
        );

        let module =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets/splitter.js");
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = dir.path().join("splitter.mjs");
        let source = format!(
            r#"
import {{ MIN_SIDE_WIDTH, DEFAULT_SIDE_WIDTH, MIN_CONVERSATION_WIDTH, STEP, BIG_STEP, SIDE_WIDTH_KEY, sideWidthBounds, clampSideWidth, parseSideWidth, keyedSideWidth, loadSideWidth, saveSideWidth }} from "{module}";

const assert = (cond, msg) => {{ if (!cond) {{ console.error("FAIL: " + msg); process.exit(1); }} }};

// The numbers app.css hard-codes.
assert(MIN_SIDE_WIDTH === 300, "floor");
assert(DEFAULT_SIDE_WIDTH === 480, "default");
assert(MIN_CONVERSATION_WIDTH === 480, "conversation floor");

// Bounds: the conversation keeps its floor, and a window too small for both
// still leaves the rail its own.
const b = sideWidthBounds(1200);
assert(b.min === 300 && b.max === 720, "bounds at 1200: " + JSON.stringify(b));
assert(sideWidthBounds(600).max === 300, "the floor wins when there is no room");

// Clamping, both ends, rounding, and garbage.
assert(clampSideWidth(100, 1200) === 300, "below the floor");
assert(clampSideWidth(5000, 1200) === 720, "above the ceiling");
assert(clampSideWidth(512.6, 1200) === 513, "rounded");
assert(clampSideWidth(NaN, 1200) === DEFAULT_SIDE_WIDTH, "garbage is the default");

// Keys move the divider the way they point.
assert(keyedSideWidth("ArrowLeft", 480, 1200) === 480 + STEP, "left widens the rail");
assert(keyedSideWidth("ArrowRight", 480, 1200) === 480 - STEP, "right narrows it");
assert(keyedSideWidth("ArrowLeft", 480, 1200, true) === 480 + BIG_STEP, "shift is a big step");
assert(keyedSideWidth("ArrowRight", 305, 1200) === 300, "stepping stops at the floor");
assert(keyedSideWidth("ArrowLeft", 715, 1200) === 720, "and at the ceiling");
assert(keyedSideWidth("Home", 480, 1200) === 300, "Home: the narrowest rail (aria-valuemin)");
assert(keyedSideWidth("End", 480, 1200) === 720, "End: the widest rail (aria-valuemax)");
assert(keyedSideWidth("a", 480, 1200) === null, "other keys are not ours");

// Parsing what storage hands back.
assert(parseSideWidth("520") === 520, "a stored width");
assert(parseSideWidth(null) === null && parseSideWidth("") === null, "nothing stored");
assert(parseSideWidth("299") === null, "below the floor is refused, not clamped");
assert(parseSideWidth("12.5") === null && parseSideWidth("wide") === null, "not a width");
assert(parseSideWidth("100000") === null, "not a width anyone chose");

// Storage round trip, reset, and a store that refuses.
const map = new Map();
const store = {{
  getItem: (k) => (map.has(k) ? map.get(k) : null),
  setItem: (k, v) => map.set(k, String(v)),
  removeItem: (k) => map.delete(k),
}};
assert(loadSideWidth(store) === null, "nothing saved yet");
saveSideWidth(store, 560);
assert(map.get(SIDE_WIDTH_KEY) === "560" && loadSideWidth(store) === 560, "saved and loaded");
saveSideWidth(store, null);
assert(!map.has(SIDE_WIDTH_KEY), "a reset forgets the choice");
const broken = {{ getItem() {{ throw new Error("denied"); }}, setItem() {{ throw new Error("denied"); }}, removeItem() {{ throw new Error("denied"); }} }};
saveSideWidth(broken, 560);
assert(loadSideWidth(broken) === null, "a refusing store is no store");
assert(loadSideWidth(null) === null, "no store at all");
console.log("ok");
"#,
            module = module.display()
        );
        std::fs::write(&driver, source).expect("write driver");

        let output = match std::process::Command::new("node").arg(&driver).output() {
            Ok(output) => output,
            // No node installed: nothing in the build depends on it.
            Err(_) => return,
        };
        assert!(
            output.status.success(),
            "splitter driver failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
