//! REST endpoints and the embedded frontend.

use std::path::PathBuf;
use std::sync::Arc;

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
}

/// Anything an endpoint can refuse to do.
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
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
    next.run(req).await
}

// -- static assets ----------------------------------------------------------

fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
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

async fn repo_branches(Query(q): Query<PathQuery>) -> ApiResult<Json<BranchInfo>> {
    let path = crate::config::expand_tilde(&q.path);
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

async fn fetch_repo(Json(q): Json<PathQuery>) -> ApiResult<Json<Value>> {
    let path = crate::config::expand_tilde(&q.path);
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
        Some(r) => crate::config::expand_tilde(r),
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
}

async fn set_permission_mode(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<PermissionModeBody>,
) -> ApiResult<Json<Value>> {
    let record = resolve(&state, &id).await?;
    state.sup.set_permission_mode(&record.id, body.mode).await?;
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
        ] {
            assert!(
                Assets::get(name).is_some(),
                "missing embedded asset: {name}"
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

    async fn test_state() -> AppState {
        let db = crate::db::Db::open_in_memory().expect("db");
        let config = Arc::new(tokio::sync::RwLock::new(Config::default()));
        AppState {
            sup: Supervisor::new(db, config),
            config_path: PathBuf::from("/dev/null"),
            port: 7717,
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

    #[tokio::test]
    async fn the_loopback_guard_lets_a_local_browser_through() {
        let request = axum::http::Request::builder()
            .uri("/api/health")
            .header("host", "127.0.0.1:7717")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(status_of(request).await, StatusCode::OK);
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
                .uri("/ws")
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
}
