//! SQLite storage: schema, agent records, the append-only event log and its
//! replay cursor.
//!
//! Every method here is synchronous. Async callers go through [`Db::run`],
//! which hands the closure to `spawn_blocking`, so no lock is ever held across
//! an `.await`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::protocol::{EventKind, PermissionRequest};
use crate::agent::state::{PermissionMode, Status};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
  id                TEXT PRIMARY KEY,
  name              TEXT NOT NULL,
  slug              TEXT NOT NULL UNIQUE,
  repo_path         TEXT NOT NULL,
  work_path         TEXT NOT NULL,
  is_git            INTEGER NOT NULL,
  branch            TEXT,
  base_ref          TEXT,
  uses_worktree     INTEGER NOT NULL,
  permission_mode   TEXT NOT NULL,
  model             TEXT,
  effort            TEXT,
  max_budget_usd    REAL,
  status            TEXT NOT NULL,
  status_detail     TEXT,
  exit_code         INTEGER,
  last_stderr       TEXT,
  cost_usd          REAL NOT NULL DEFAULT 0,
  created_at        INTEGER NOT NULL,
  last_active_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
  agent_id  TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  seq       INTEGER NOT NULL,
  ts        INTEGER NOT NULL,
  kind      TEXT NOT NULL,
  payload   TEXT NOT NULL,
  PRIMARY KEY (agent_id, seq)
);

CREATE TABLE IF NOT EXISTS repo_usage (
  path TEXT PRIMARY KEY,
  last_used_at INTEGER NOT NULL
);

-- The account's last known usage against its rate-limit windows. One row: it
-- describes the account, not an agent, and every agent's CLI reports the same
-- thing. Kept because it is the one gauge whose value does not expire with the
-- process — `resetsAt` is absolute, so a snapshot taken before a restart still
-- says something true afterwards, and a server that has just come up has no
-- other way to know it is rate-limited until an agent runs and finds out.
CREATE TABLE IF NOT EXISTS rate_limit (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  captured_at INTEGER NOT NULL,
  payload     TEXT NOT NULL
);
"#;

/// Additive migrations applied after [`SCHEMA`].
///
/// `add_dirs` is not in the design's §3 table: `--add-dir` values have to
/// outlive the process or a resume after a server restart silently drops
/// them. `branch_is_new` records whether we created the agent's
/// branch, which decides whether deleting the agent may delete it. Both are
/// added, never required, so an older database opens unchanged.
fn migrate(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(agents)")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !existing.iter().any(|c| c == "add_dirs") {
        conn.execute("ALTER TABLE agents ADD COLUMN add_dirs TEXT", [])
            .context("adding the add_dirs column")?;
    }
    // Every agent that predates the column created its own branch — reuse did
    // not exist — so a NULL reads as `true`, not as the safer-looking `false`.
    if !existing.iter().any(|c| c == "branch_is_new") {
        conn.execute("ALTER TABLE agents ADD COLUMN branch_is_new INTEGER", [])
            .context("adding the branch_is_new column")?;
    }
    Ok(())
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A row of the `agents` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub repo_path: String,
    pub work_path: String,
    pub is_git: bool,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    pub uses_worktree: bool,
    /// We created `branch` for this agent, so deleting the agent may delete it.
    /// False when the agent was pointed at a branch that already existed, which
    /// is not ours to destroy.
    #[serde(default = "default_true")]
    pub branch_is_new: bool,
    pub permission_mode: PermissionMode,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_budget_usd: Option<f64>,
    /// `--add-dir` values. Stored so a resume keeps them.
    #[serde(default)]
    pub add_dirs: Vec<String>,
    pub status: Status,
    pub status_detail: Option<String>,
    pub exit_code: Option<i64>,
    pub last_stderr: Option<String>,
    pub cost_usd: f64,
    pub created_at: i64,
    pub last_active_at: i64,
}

fn default_true() -> bool {
    true
}

/// A row of the `events` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub agent_id: String,
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    /// The raw JSON payload, reparsed so the browser gets structure, not a string.
    pub payload: Value,
}

/// A handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    /// An in-memory database, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("opening in-memory database")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign keys")?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a blocking database closure off the async runtime.
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Db) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .context("database task panicked")?
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        f(&guard)
    }

    // -- agents -------------------------------------------------------------

    pub fn insert_agent(&self, a: &AgentRecord) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO agents (id, name, slug, repo_path, work_path, is_git, branch,
                     base_ref, uses_worktree, permission_mode, model, effort, max_budget_usd,
                     status, status_detail, exit_code, last_stderr, cost_usd, created_at,
                     last_active_at, add_dirs, branch_is_new)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21, ?22)",
                params![
                    a.id,
                    a.name,
                    a.slug,
                    a.repo_path,
                    a.work_path,
                    a.is_git as i64,
                    a.branch,
                    a.base_ref,
                    a.uses_worktree as i64,
                    a.permission_mode.as_str(),
                    a.model,
                    a.effort,
                    a.max_budget_usd,
                    a.status.as_str(),
                    a.status_detail,
                    a.exit_code,
                    a.last_stderr,
                    a.cost_usd,
                    a.created_at,
                    a.last_active_at,
                    serde_json::to_string(&a.add_dirs).unwrap_or_else(|_| "[]".to_string()),
                    a.branch_is_new as i64,
                ],
            )
            .context("inserting agent")?;
            Ok(())
        })
    }

    pub fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {AGENT_COLUMNS} FROM agents ORDER BY last_active_at DESC"
            ))?;
            let rows = stmt.query_map([], row_to_agent)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn get_agent(&self, id: &str) -> Result<Option<AgentRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE id = ?1"),
                    params![id],
                    row_to_agent,
                )
                .optional()?)
        })
    }

    pub fn get_agent_by_slug(&self, slug: &str) -> Result<Option<AgentRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE slug = ?1"),
                    params![slug],
                    row_to_agent,
                )
                .optional()?)
        })
    }

    /// All slugs and branch names in use, for collision suffixing.
    pub fn taken_names(&self) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT slug FROM agents UNION SELECT branch FROM agents")?;
            let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
            let mut out = Vec::new();
            for r in rows {
                if let Some(v) = r? {
                    out.push(v);
                }
            }
            Ok(out)
        })
    }

    pub fn set_status(
        &self,
        id: &str,
        status: Status,
        detail: Option<&str>,
        exit_code: Option<i64>,
        last_stderr: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE agents SET status = ?2, status_detail = ?3, exit_code = ?4,
                     last_stderr = COALESCE(?5, last_stderr), last_active_at = ?6
                 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    detail,
                    exit_code,
                    last_stderr,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_status_detail(&self, id: &str, detail: Option<&str>) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE agents SET status_detail = ?2, last_active_at = ?3 WHERE id = ?1",
                params![id, detail, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn set_cost(&self, id: &str, cost_usd: f64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE agents SET cost_usd = ?2, last_active_at = ?3 WHERE id = ?1",
                params![id, cost_usd, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn set_permission_mode(&self, id: &str, mode: PermissionMode) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE agents SET permission_mode = ?2 WHERE id = ?1",
                params![id, mode.as_str()],
            )?;
            Ok(())
        })
    }

    pub fn rename_agent(&self, id: &str, name: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE agents SET name = ?2 WHERE id = ?1",
                params![id, name],
            )?;
            Ok(())
        })
    }

    pub fn delete_agent(&self, id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM events WHERE agent_id = ?1", params![id])?;
            conn.execute("DELETE FROM agents WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Mark every agent that looks alive as `Stopped`, so Resume works after a
    /// server restart. Agents do not survive server death (§10).
    pub fn mark_all_stopped(&self) -> Result<usize> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE agents SET status = 'stopped', status_detail = NULL
                 WHERE status NOT IN ('stopped', 'failed')",
                [],
            )?;
            Ok(n)
        })
    }

    // -- events -------------------------------------------------------------

    /// Append an event, returning its per-agent sequence number.
    pub fn append_event(&self, agent_id: &str, kind: EventKind, payload: &Value) -> Result<i64> {
        let text = serde_json::to_string(payload).context("serialising event payload")?;
        self.with_conn(|conn| {
            let seq: i64 = conn.query_row(
                "INSERT INTO events (agent_id, seq, ts, kind, payload)
                 VALUES (?1,
                     (SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE agent_id = ?1),
                     ?2, ?3, ?4)
                 RETURNING seq",
                params![agent_id, now_ms(), kind.as_str(), text],
                |r| r.get(0),
            )?;
            Ok(seq)
        })
    }

    /// The replay cursor query: everything after `after_seq`, oldest first.
    pub fn events_after(
        &self,
        agent_id: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<EventRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT agent_id, seq, ts, kind, payload FROM events
                 WHERE agent_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![agent_id, after_seq, limit], row_to_event)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn max_seq(&self, agent_id: &str) -> Result<i64> {
        self.with_conn(|conn| {
            let seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE agent_id = ?1",
                params![agent_id],
                |r| r.get(0),
            )?;
            Ok(seq)
        })
    }

    /// The cursor a fresh page should start from so it receives the last
    /// `window` events. Same `seq > ?` query either way (§7).
    pub fn tail_cursor(&self, agent_id: &str, window: i64) -> Result<i64> {
        Ok((self.max_seq(agent_id)? - window).max(0))
    }

    /// Permission requests with no matching decision, per agent.
    pub fn pending_permissions(&self, agent_id: &str) -> Result<Vec<PermissionRequest>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, payload FROM events
                 WHERE agent_id = ?1 AND kind IN ('permission_request', 'permission_decision')
                 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![agent_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut pending: Vec<PermissionRequest> = Vec::new();
            for row in rows {
                let (kind, payload) = row?;
                let value: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let request_id = value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if kind == "permission_request" {
                    if let Ok(req) = serde_json::from_value::<PermissionRequest>(value) {
                        pending.push(req);
                    }
                } else {
                    pending.retain(|p| p.request_id != request_id);
                }
            }
            Ok(pending)
        })
    }

    /// Close every outstanding permission request for an agent.
    ///
    /// A request id belongs to the process that asked; once that process is
    /// gone the request can never be answered, so it is recorded as expired
    /// rather than left to haunt the next launch (§5).
    pub fn expire_pending_permissions(&self, agent_id: &str) -> Result<Vec<String>> {
        let pending = self.pending_permissions(agent_id)?;
        let mut expired = Vec::new();
        for request in pending {
            self.append_event(
                agent_id,
                EventKind::PermissionDecision,
                &serde_json::json!({
                    "request_id": request.request_id,
                    "behavior": "expired",
                    "tool_name": request.tool_name,
                }),
            )?;
            expired.push(request.request_id);
        }
        Ok(expired)
    }

    // -- repo usage ---------------------------------------------------------

    pub fn touch_repo(&self, path: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO repo_usage (path, last_used_at) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET last_used_at = ?2",
                params![path, now_ms()],
            )?;
            Ok(())
        })
    }

    // -- account rate limits -------------------------------------------------

    /// Replace the stored snapshot. Called once per API request the CLI makes,
    /// so it is a single small upsert and nothing more.
    pub fn set_rate_limit(&self, info: &Value, captured_at: i64) -> Result<()> {
        let payload = serde_json::to_string(info).context("serialising the rate limit")?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO rate_limit (id, captured_at, payload) VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET captured_at = ?1, payload = ?2",
                rusqlite::params![captured_at, payload],
            )?;
            Ok(())
        })
    }

    /// The stored snapshot and when it was taken, if there is one.
    pub fn rate_limit(&self) -> Result<Option<(i64, Value)>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT captured_at, payload FROM rate_limit WHERE id = 1")?;
            let mut rows = stmt.query([])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let captured_at: i64 = row.get(0)?;
            let payload: String = row.get(1)?;
            // A snapshot we cannot parse is one written by a different build.
            // Dropping it is right: it is a cache, not a record.
            Ok(serde_json::from_str(&payload)
                .ok()
                .map(|value| (captured_at, value)))
        })
    }

    pub fn repo_usage(&self) -> Result<HashMap<String, i64>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT path, last_used_at FROM repo_usage")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            let mut out = HashMap::new();
            for r in rows {
                let (k, v) = r?;
                out.insert(k, v);
            }
            Ok(out)
        })
    }
}

const AGENT_COLUMNS: &str = "id, name, slug, repo_path, work_path, is_git, branch, base_ref, \
     uses_worktree, permission_mode, model, effort, max_budget_usd, status, status_detail, \
     exit_code, last_stderr, cost_usd, created_at, last_active_at, add_dirs, branch_is_new";

fn row_to_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRecord> {
    let mode: String = row.get(9)?;
    let status: String = row.get(13)?;
    Ok(AgentRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        repo_path: row.get(3)?,
        work_path: row.get(4)?,
        is_git: row.get::<_, i64>(5)? != 0,
        branch: row.get(6)?,
        base_ref: row.get(7)?,
        uses_worktree: row.get::<_, i64>(8)? != 0,
        // An unreadable enum falls back rather than failing the whole query.
        permission_mode: mode.parse().unwrap_or(PermissionMode::Ask),
        model: row.get(10)?,
        effort: row.get(11)?,
        max_budget_usd: row.get(12)?,
        status: status.parse().unwrap_or(Status::Failed),
        status_detail: row.get(14)?,
        exit_code: row.get(15)?,
        last_stderr: row.get(16)?,
        cost_usd: row.get(17)?,
        created_at: row.get(18)?,
        last_active_at: row.get(19)?,
        add_dirs: row
            .get::<_, Option<String>>(20)?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default(),
        branch_is_new: row
            .get::<_, Option<i64>>(21)?
            .map(|v| v != 0)
            .unwrap_or(true),
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let payload: String = row.get(4)?;
    Ok(EventRecord {
        agent_id: row.get(0)?,
        seq: row.get(1)?,
        ts: row.get(2)?,
        kind: row.get(3)?,
        payload: serde_json::from_str(&payload).unwrap_or_else(|_| Value::String(payload.clone())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_agent(id: &str, slug: &str) -> AgentRecord {
        AgentRecord {
            id: id.to_string(),
            name: "Fix the parser".to_string(),
            slug: slug.to_string(),
            repo_path: "/repos/thing".to_string(),
            work_path: "/repos/.worktrees/thing/fix".to_string(),
            is_git: true,
            branch: Some("sw_fix_the_parser".to_string()),
            base_ref: Some("main".to_string()),
            uses_worktree: true,
            branch_is_new: true,
            permission_mode: PermissionMode::Ask,
            model: Some("opus".to_string()),
            effort: None,
            max_budget_usd: None,
            add_dirs: vec!["/extra/one".to_string()],
            status: Status::Starting,
            status_detail: None,
            exit_code: None,
            last_stderr: None,
            cost_usd: 0.0,
            created_at: 1,
            last_active_at: 1,
        }
    }

    #[test]
    fn agents_round_trip() {
        let db = Db::open_in_memory().expect("db");
        let a = sample_agent("id-1", "fix_the_parser");
        db.insert_agent(&a).expect("insert");
        let back = db.get_agent("id-1").expect("get").expect("present");
        assert_eq!(a, back);
        let by_slug = db
            .get_agent_by_slug("fix_the_parser")
            .expect("get")
            .expect("present");
        assert_eq!(by_slug.id, "id-1");
        assert_eq!(db.list_agents().expect("list").len(), 1);
    }

    #[test]
    fn opens_a_database_file_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state").join("agents.db");
        let db = Db::open(&path).expect("open");
        db.insert_agent(&sample_agent("id-1", "s")).expect("insert");
        drop(db);
        let reopened = Db::open(&path).expect("reopen");
        assert_eq!(reopened.list_agents().expect("list").len(), 1);
    }

    #[test]
    fn status_and_cost_updates_stick() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("id-1", "s")).expect("insert");
        db.set_status(
            "id-1",
            Status::Working,
            Some("Bash: cargo test"),
            None,
            None,
        )
        .expect("status");
        let a = db.get_agent("id-1").expect("get").expect("present");
        assert_eq!(a.status, Status::Working);
        assert_eq!(a.status_detail.as_deref(), Some("Bash: cargo test"));

        db.set_status("id-1", Status::Failed, None, Some(2), Some("boom"))
            .expect("status");
        let a = db.get_agent("id-1").expect("get").expect("present");
        assert_eq!(a.status, Status::Failed);
        assert_eq!(a.exit_code, Some(2));
        assert_eq!(a.last_stderr.as_deref(), Some("boom"));

        db.set_cost("id-1", 1.25).expect("cost");
        db.rename_agent("id-1", "New name").expect("rename");
        let a = db.get_agent("id-1").expect("get").expect("present");
        assert_eq!(a.cost_usd, 1.25);
        assert_eq!(a.name, "New name");
        // Rename never touches the slug or branch.
        assert_eq!(a.slug, "s");
        assert_eq!(a.branch.as_deref(), Some("sw_fix_the_parser"));
    }

    #[test]
    fn mark_all_stopped_leaves_terminal_agents_alone() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        let mut failed = sample_agent("b", "b");
        failed.status = Status::Failed;
        failed.exit_code = Some(1);
        db.insert_agent(&failed).expect("insert");

        assert_eq!(db.mark_all_stopped().expect("mark"), 1);
        assert_eq!(
            db.get_agent("a").expect("get").expect("present").status,
            Status::Stopped
        );
        let b = db.get_agent("b").expect("get").expect("present");
        assert_eq!(b.status, Status::Failed);
        assert_eq!(b.exit_code, Some(1));
    }

    #[test]
    fn event_seq_is_monotonic_per_agent() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        db.insert_agent(&sample_agent("b", "b")).expect("insert");

        for i in 1..=3 {
            let seq = db
                .append_event("a", EventKind::Assistant, &json!({"i": i}))
                .expect("append");
            assert_eq!(seq, i);
        }
        let seq = db
            .append_event("b", EventKind::User, &json!({"i": 1}))
            .expect("append");
        assert_eq!(seq, 1, "sequences are per agent, not global");
        assert_eq!(db.max_seq("a").expect("max"), 3);
        assert_eq!(db.max_seq("b").expect("max"), 1);
        assert_eq!(db.max_seq("missing").expect("max"), 0);
    }

    #[test]
    fn the_rate_limit_snapshot_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents.db");
        let info = json!({
            "status": "rejected",
            "resetsAt": 1787846400i64,
            "unifiedWindows": {"five_hour": {"utilization": 0.97, "resetsAt": 1787846400i64}},
        });

        {
            let db = Db::open(&path).expect("open");
            assert!(db.rate_limit().expect("read").is_none(), "empty to start");
            db.set_rate_limit(&info, 1_787_800_000_000).expect("write");
            // One row, however many times it is written: it describes the
            // account, and the newest reading replaces the last.
            db.set_rate_limit(&info, 1_787_800_001_000)
                .expect("rewrite");
        }

        // A restart is the whole point: a fresh process must find it.
        let db = Db::open(&path).expect("reopen");
        let (captured_at, stored) = db.rate_limit().expect("read").expect("present");
        assert_eq!(captured_at, 1_787_800_001_000, "the newest capture wins");
        assert_eq!(stored, info);
        assert_eq!(
            db.with_conn(
                |c| Ok(c.query_row("SELECT count(*) FROM rate_limit", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .expect("count"),
            1,
            "the table holds one row, not one per reading"
        );
    }

    #[test]
    fn cursor_query_returns_only_the_delta() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        for i in 1..=10 {
            db.append_event("a", EventKind::Assistant, &json!({"i": i}))
                .expect("append");
        }

        let all = db.events_after("a", 0, 500).expect("query");
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[9].seq, 10);
        assert_eq!(all[0].payload["i"], json!(1));
        assert_eq!(all[0].kind, "assistant");

        let delta = db.events_after("a", 7, 500).expect("query");
        assert_eq!(delta.len(), 3);
        assert_eq!(
            delta.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );

        // Cursor at the head yields nothing; a cursor past the head is harmless.
        assert!(db.events_after("a", 10, 500).expect("query").is_empty());
        assert!(db.events_after("a", 99, 500).expect("query").is_empty());

        // The limit truncates from the oldest end of the window.
        let limited = db.events_after("a", 0, 4).expect("query");
        assert_eq!(
            limited.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn tail_cursor_windows_the_last_n_events() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        for i in 1..=12 {
            db.append_event("a", EventKind::Assistant, &json!({"i": i}))
                .expect("append");
        }
        let cursor = db.tail_cursor("a", 5).expect("cursor");
        assert_eq!(cursor, 7);
        let page = db.events_after("a", cursor, 500).expect("query");
        assert_eq!(page.len(), 5);
        assert_eq!(page[0].seq, 8);

        // Fewer events than the window: start from the beginning.
        assert_eq!(db.tail_cursor("a", 500).expect("cursor"), 0);
    }

    #[test]
    fn deleting_an_agent_takes_its_events() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        db.append_event("a", EventKind::System, &json!({}))
            .expect("append");
        db.delete_agent("a").expect("delete");
        assert!(db.get_agent("a").expect("get").is_none());
        assert!(db.events_after("a", 0, 500).expect("query").is_empty());
    }

    #[test]
    fn pending_permissions_drop_once_decided() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");

        let req = |id: &str| {
            json!({
                "request_id": id,
                "tool_name": "Write",
                "input": {"file_path": "/tmp/x"},
            })
        };
        db.append_event("a", EventKind::PermissionRequest, &req("1"))
            .expect("append");
        db.append_event("a", EventKind::PermissionRequest, &req("2"))
            .expect("append");
        assert_eq!(db.pending_permissions("a").expect("pending").len(), 2);

        db.append_event(
            "a",
            EventKind::PermissionDecision,
            &json!({"request_id": "1", "behavior": "allow"}),
        )
        .expect("append");
        let pending = db.pending_permissions("a").expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, "2");
        assert_eq!(pending[0].tool_name, "Write");
    }

    #[test]
    fn repo_usage_is_recorded() {
        let db = Db::open_in_memory().expect("db");
        assert!(db.repo_usage().expect("usage").is_empty());
        db.touch_repo("/repos/one").expect("touch");
        db.touch_repo("/repos/one").expect("touch again");
        db.touch_repo("/repos/two").expect("touch");
        let usage = db.repo_usage().expect("usage");
        assert_eq!(usage.len(), 2);
        assert!(usage.contains_key("/repos/one"));
    }

    #[test]
    fn taken_names_covers_slugs_and_branches() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "slug_a"))
            .expect("insert");
        let names = db.taken_names().expect("names");
        assert!(names.contains(&"slug_a".to_string()));
        assert!(names.contains(&"sw_fix_the_parser".to_string()));
    }

    #[tokio::test]
    async fn run_executes_off_thread() {
        let db = Db::open_in_memory().expect("db");
        db.run(|db| db.insert_agent(&sample_agent("a", "a")))
            .await
            .expect("insert");
        let agents = db.run(|db| db.list_agents()).await.expect("list");
        assert_eq!(agents.len(), 1);
    }

    #[test]
    fn add_dirs_survive_a_round_trip() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        let back = db.get_agent("a").expect("get").expect("present");
        assert_eq!(back.add_dirs, vec!["/extra/one".to_string()]);
    }

    #[test]
    fn an_older_database_without_add_dirs_is_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old.db");
        {
            // The §3 schema exactly as it shipped, with no add_dirs column.
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE agents (
                   id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
                   repo_path TEXT NOT NULL, work_path TEXT NOT NULL, is_git INTEGER NOT NULL,
                   branch TEXT, base_ref TEXT, uses_worktree INTEGER NOT NULL,
                   permission_mode TEXT NOT NULL, model TEXT, effort TEXT, max_budget_usd REAL,
                   status TEXT NOT NULL, status_detail TEXT, exit_code INTEGER, last_stderr TEXT,
                   cost_usd REAL NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
                   last_active_at INTEGER NOT NULL);
                 INSERT INTO agents (id, name, slug, repo_path, work_path, is_git, uses_worktree,
                   permission_mode, status, cost_usd, created_at, last_active_at)
                 VALUES ('old', 'Old agent', 'old', '/r', '/r', 1, 0, 'ask', 'stopped', 0, 1, 1);",
            )
            .expect("legacy schema");
        }
        let db = Db::open(&path).expect("open and migrate");
        let agent = db.get_agent("old").expect("get").expect("present");
        assert_eq!(agent.name, "Old agent");
        assert!(agent.add_dirs.is_empty());
        // And the migration is idempotent.
        drop(db);
        assert!(Db::open(&path).is_ok());
    }

    #[test]
    fn expiring_pending_permissions_closes_them_for_good() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        for id in ["1", "2"] {
            db.append_event(
                "a",
                EventKind::PermissionRequest,
                &json!({"request_id": id, "tool_name": "Write", "input": {}}),
            )
            .expect("append");
        }
        assert_eq!(db.pending_permissions("a").expect("pending").len(), 2);

        let expired = db.expire_pending_permissions("a").expect("expire");
        assert_eq!(expired, vec!["1".to_string(), "2".to_string()]);
        assert!(db.pending_permissions("a").expect("pending").is_empty());
        // Expiry is recorded in the log, not hidden.
        let events = db.events_after("a", 0, 100).expect("events");
        let decisions: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "permission_decision")
            .collect();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].payload["behavior"], json!("expired"));
        // A second pass is a no-op.
        assert!(
            db.expire_pending_permissions("a")
                .expect("expire")
                .is_empty()
        );
    }

    #[test]
    fn a_truncated_replay_page_can_be_detected() {
        let db = Db::open_in_memory().expect("db");
        db.insert_agent(&sample_agent("a", "a")).expect("insert");
        for i in 1..=3000 {
            db.append_event("a", EventKind::Assistant, &json!({"i": i}))
                .expect("append");
        }
        // A client reconnecting from an old cursor gets one page, and the head
        // is further on: the reply has to admit there is more.
        let page = db.events_after("a", 100, 500).expect("query");
        let cursor = page.last().map(|e| e.seq).unwrap_or(100);
        assert_eq!(cursor, 600);
        assert!(cursor < db.max_seq("a").expect("max"));

        // Walking forward from the returned cursor eventually catches up.
        let mut cursor = cursor;
        while cursor < db.max_seq("a").expect("max") {
            let page = db.events_after("a", cursor, 500).expect("query");
            assert!(!page.is_empty(), "the walk must make progress");
            cursor = page.last().map(|e| e.seq).unwrap_or(cursor);
        }
        assert_eq!(cursor, 3000);
    }
}
