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
"#;

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
    pub permission_mode: PermissionMode,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub status: Status,
    pub status_detail: Option<String>,
    pub exit_code: Option<i64>,
    pub last_stderr: Option<String>,
    pub cost_usd: f64,
    pub created_at: i64,
    pub last_active_at: i64,
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
                     last_active_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20)",
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
     exit_code, last_stderr, cost_usd, created_at, last_active_at";

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
            permission_mode: PermissionMode::Ask,
            model: Some("opus".to_string()),
            effort: None,
            max_budget_usd: None,
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
}
