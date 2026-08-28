//! REST endpoints and the embedded frontend.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::protocol::PermissionDecision;
use crate::agent::state::PermissionMode;
use crate::agent::supervisor::{DeleteError, ServerMsg, SpawnRequest, Supervisor};
use crate::config::Config;
use crate::repo::{clone, git, scan};

/// Events handed to a fresh page load before it starts streaming (§7).
pub const REPLAY_WINDOW: i64 = 500;

#[derive(rust_embed::Embed)]
#[folder = "src/assets/"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub sup: Arc<Supervisor>,
    pub config_path: PathBuf,
    /// The port we are listening on, for the loopback `Host` check.
    pub port: u16,
    /// The per-boot token every API call and socket upgrade must carry.
    pub token: Arc<SessionToken>,
    /// Throttle for the refusal log, so one stuck page cannot drown it.
    pub refusals: Arc<RefusalLog>,
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
    pub fn note(&self, path: &str) {
        if let Some(suppressed) = self.tally(path, Instant::now()) {
            if suppressed == 0 {
                tracing::warn!(%path, "refused a request with no valid session token");
            } else {
                tracing::warn!(
                    %path,
                    suppressed,
                    "repeatedly refused requests with no valid session token; a page is likely \
                     retrying with a token from before the last restart"
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
        .route("/ws", get(super::ws::handler))
        .route("/api/health", get(health))
        // Loopback binding alone does not survive DNS rebinding: a page on
        // http://evil.example:7717 rebound to 127.0.0.1 would otherwise reach
        // every endpoint here, including spawning an agent in `dangerous` mode.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard_loopback,
        ))
        .with_state(state)
}

/// Hosts we answer to: loopback, on the port we are actually serving.
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
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
    let name_ok = matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1");
    let port_ok = match given_port {
        Some(p) => p == port,
        // A missing port means the scheme default.
        None => port == 80,
    };
    name_ok && port_ok
}

/// Origins we accept. Absent is fine — that is a non-browser client, which
/// cannot be a rebinding victim; present and foreign is not.
pub fn origin_allowed(origin: Option<&str>, port: u16) -> bool {
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
    host_allowed(Some(rest), port)
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

async fn guard_loopback(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !host_allowed(host, state.port) {
        tracing::warn!(?host, "refused a request with a non-loopback Host header");
        return (
            StatusCode::FORBIDDEN,
            "claude-web only answers to a loopback Host header",
        )
            .into_response();
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin_allowed(origin, state.port) {
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
        let presented = token_of(&req);
        if !presented.is_some_and(|t| state.token.matches(&t)) {
            state.refusals.note(&path);
            return ApiError::unauthorized(
                "This request needs the session token. Open the link claude-web printed at \
                 startup — the token changes every time the server restarts.",
            )
            .into_response();
        }
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
    let info = tokio::task::spawn_blocking(move || {
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
    let view = state.sup.view(record).await?;
    Ok(Json(json!({ "agent": view })))
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
}

async fn post_message(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<MessageBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state.sup.send_message(&record.id, &body.text).await?;
    Ok(Json(json!({"ok": true})))
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

async fn set_permission_mode(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<PermissionModeBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state
        .sup
        .set_permission_mode(&record.id, body.mode, body.confirm)
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
    AxPath(id): AxPath<String>,
    Json(body): Json<PermissionBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    let decision = decision_from(&body.behavior, body.message, body.updated_input)
        .ok_or_else(|| ApiError::bad_request(format!("unknown behavior: {}", body.behavior)))?;
    state
        .sup
        .decide(&record.id, &body.request_id, decision)
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
        ] {
            assert!(
                Assets::get(name).is_some(),
                "missing embedded asset: {name}"
            );
        }
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

    #[tokio::test]
    async fn missing_assets_are_a_404_not_a_panic() {
        let response = serve_embedded("nope.js");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_loopback_hosts_are_answered() {
        for host in [
            "127.0.0.1:7717",
            "localhost:7717",
            "[::1]:7717",
            " localhost:7717 ",
        ] {
            assert!(host_allowed(Some(host), 7717), "{host} must be allowed");
        }
        for host in [
            "evil.example:7717",
            "attacker.test:7717",
            "127.0.0.1.nip.io:7717",
            "127.0.0.1:9999",
            "localhost",
            "192.168.1.5:7717",
        ] {
            assert!(!host_allowed(Some(host), 7717), "{host} must be refused");
        }
        assert!(!host_allowed(None, 7717), "a missing Host is refused");
        assert!(host_allowed(Some("localhost"), 80));
    }

    #[test]
    fn only_loopback_origins_are_accepted() {
        assert!(
            origin_allowed(None, 7717),
            "non-browser clients send no Origin"
        );
        assert!(origin_allowed(Some("http://127.0.0.1:7717"), 7717));
        assert!(origin_allowed(Some("http://localhost:7717"), 7717));
        for origin in [
            "http://evil.example",
            "https://evil.example:7717",
            "http://localhost:3000",
            "null",
            "file://",
        ] {
            assert!(
                !origin_allowed(Some(origin), 7717),
                "{origin} must be refused"
            );
        }
    }

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn test_state() -> AppState {
        let db = crate::db::Db::open_in_memory().expect("db");
        let config = Arc::new(tokio::sync::RwLock::new(Config::default()));
        AppState {
            sup: Supervisor::new(db, config),
            config_path: PathBuf::from("/dev/null"),
            port: 7717,
            token: Arc::new(SessionToken(TEST_TOKEN.to_string())),
            refusals: Default::default(),
        }
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
        use tower::ServiceExt;
        router(test_state().await)
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

    #[tokio::test]
    async fn every_api_route_needs_the_token() {
        for (method, path) in [
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
        ] {
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
}
