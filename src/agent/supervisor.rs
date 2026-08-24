//! The agent registry: spawn, supervise, interrupt, stop, resume, delete.
//!
//! One [`Runner`] task owns each child process. Everything else talks to it
//! through a command channel, so the child's stdin has exactly one writer and
//! the status state machine has exactly one owner.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{RwLock, broadcast, mpsc};

use crate::config::Config;
use crate::db::{AgentRecord, Db, now_ms};
use crate::repo::git;

use super::process::{self, Action, ChildHandle, ExitInfo, ProcessMsg, SpawnConfig};
use super::protocol::{
    self, EventKind, LaunchArgs, PermissionDecision, PermissionRequest, SlashCommand,
};
use super::state::{PermissionMode, Status, Transition};

/// How long a child gets between SIGTERM and SIGKILL (§4).
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// Events pushed to every connected browser. The envelope is tagged with the
/// agent id so one socket serves the dashboard and every detail view (§7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Event {
        agent_id: String,
        seq: i64,
        ts: i64,
        kind: String,
        payload: Value,
    },
    Status {
        agent_id: String,
        status: Status,
        status_detail: Option<String>,
        exit_code: Option<i64>,
        last_stderr: Option<String>,
        cost_usd: f64,
    },
    PermissionRequest {
        agent_id: String,
        request: PermissionRequest,
    },
    PermissionResolved {
        agent_id: String,
        request_id: String,
        behavior: String,
    },
    Partial {
        agent_id: String,
        payload: Value,
    },
    Commands {
        agent_id: String,
        commands: Vec<SlashCommand>,
    },
    Queued {
        agent_id: String,
        still_queued: Value,
    },
    AgentAdded {
        agent: Box<AgentRecord>,
    },
    AgentRemoved {
        agent_id: String,
    },
    CloneProgress {
        clone_id: String,
        line: String,
    },
    CloneDone {
        clone_id: String,
        path: Option<String>,
        error: Option<String>,
    },
    /// Something worth telling the operator about: an unrecognised protocol
    /// event, a version mismatch, a failed git call.
    Notice {
        agent_id: Option<String>,
        level: String,
        text: String,
    },
}

/// What the spawn form sends.
#[derive(Debug, Clone, Deserialize)]
pub struct SpawnRequest {
    pub repo_path: String,
    pub task_name: String,
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
    /// Work in the main checkout instead of an isolated worktree (§6).
    #[serde(default)]
    pub in_place: bool,
    #[serde(default)]
    pub add_dirs: Vec<String>,
    #[serde(default)]
    pub first_message: Option<String>,
}

/// The answer to a spawn, including the soft-cap warning.
#[derive(Debug, Clone, Serialize)]
pub struct SpawnOutcome {
    pub agent: AgentRecord,
    pub warning: Option<String>,
}

/// An agent plus the live extras the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct AgentView {
    #[serde(flatten)]
    pub record: AgentRecord,
    pub running: bool,
    /// A terminal agent with no child process can be resumed (F7).
    pub resumable: bool,
    pub pending_permissions: Vec<PermissionRequest>,
    pub commands: Vec<SlashCommand>,
}

/// What a delete would destroy, when it is refused.
#[derive(Debug, Clone, Serialize)]
pub struct DeleteRefusal {
    pub report: git::SafetyReport,
    pub message: String,
}

#[derive(Debug)]
enum AgentCommand {
    Send(String),
    Decide {
        request_id: String,
        decision: PermissionDecision,
    },
    Interrupt,
    SetPermissionMode(PermissionMode),
    Stop,
}

struct RunnerHandle {
    tx: mpsc::UnboundedSender<AgentCommand>,
    commands: Arc<RwLock<Vec<SlashCommand>>>,
    /// Which launch this handle belongs to. A runner only ever deregisters its
    /// own generation, so a stale task cannot evict a live one.
    generation: u64,
}

/// The registry of live agents.
pub struct Supervisor {
    db: Db,
    config: Arc<RwLock<Config>>,
    runners: Arc<RwLock<HashMap<String, RunnerHandle>>>,
    next_generation: AtomicU64,
    bus: broadcast::Sender<ServerMsg>,
}

impl Supervisor {
    pub fn new(db: Db, config: Arc<RwLock<Config>>) -> Arc<Self> {
        let (bus, _) = broadcast::channel(2048);
        Arc::new(Self {
            db,
            config,
            runners: Arc::new(RwLock::new(HashMap::new())),
            next_generation: AtomicU64::new(1),
            bus,
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerMsg> {
        self.bus.subscribe()
    }

    pub fn broadcast(&self, msg: ServerMsg) {
        // A send error only means nobody is listening.
        let _ = self.bus.send(msg);
    }

    pub async fn config(&self) -> Config {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, cfg: Config) {
        *self.config.write().await = cfg;
    }

    pub async fn is_running(&self, id: &str) -> bool {
        self.runners.read().await.contains_key(id)
    }

    pub async fn running_count(&self) -> usize {
        self.runners.read().await.len()
    }

    /// Every agent with its live extras.
    pub async fn list(&self) -> Result<Vec<AgentView>> {
        let records = self.db.run(|db| db.list_agents()).await?;
        let mut out = Vec::with_capacity(records.len());
        for record in records {
            out.push(self.view(record).await?);
        }
        Ok(out)
    }

    pub async fn view(&self, record: AgentRecord) -> Result<AgentView> {
        let id = record.id.clone();
        let runners = self.runners.read().await;
        let (running, commands) = match runners.get(&id) {
            Some(handle) => (true, handle.commands.read().await.clone()),
            None => (false, Vec::new()),
        };
        drop(runners);
        // A dead process cannot be answered: its request ids died with it, so a
        // stopped agent shows no pending approvals even though they are still
        // on the event log.
        let pending = if running {
            let id = id.clone();
            self.db
                .run(move |db| db.pending_permissions(&id))
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(AgentView {
            resumable: !running && record.status.is_terminal(),
            running: running && record.status.is_live(),
            record,
            pending_permissions: pending,
            commands,
        })
    }

    // -- spawning -----------------------------------------------------------

    /// Create an agent: allocate names, prepare the worktree, launch the child.
    pub async fn spawn_agent(&self, req: SpawnRequest) -> Result<SpawnOutcome> {
        let cfg = self.config().await;
        let repo_path = crate::config::expand_tilde(&req.repo_path);
        if !repo_path.is_dir() {
            bail!("{} is not a directory", repo_path.display());
        }

        let taken_slugs: HashSet<String> = self
            .db
            .run(|db| db.taken_names())
            .await?
            .into_iter()
            .collect();

        let task_name = if req.task_name.trim().is_empty() {
            repo_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "agent".to_string())
        } else {
            req.task_name.trim().to_string()
        };

        let prefix = cfg.branch_prefix.clone();
        let repo_for_prep = repo_path.clone();
        let req_for_prep = req.clone();
        let name_for_prep = task_name.clone();
        // All git and filesystem work happens off the runtime.
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_workspace(
                &repo_for_prep,
                &name_for_prep,
                &prefix,
                &taken_slugs,
                &req_for_prep,
            )
        })
        .await
        .context("workspace preparation panicked")??;

        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let record = AgentRecord {
            id: id.clone(),
            name: task_name,
            slug: prepared.slug,
            repo_path: repo_path.to_string_lossy().to_string(),
            work_path: prepared.work_path.to_string_lossy().to_string(),
            is_git: prepared.is_git,
            branch: prepared.branch,
            base_ref: prepared.base_ref,
            uses_worktree: prepared.uses_worktree,
            permission_mode: req.permission_mode.unwrap_or(cfg.default_permission_mode),
            model: req
                .model
                .clone()
                .or_else(|| Some(cfg.default_model.clone())),
            effort: req.effort.clone(),
            max_budget_usd: req.max_budget_usd,
            add_dirs: req
                .add_dirs
                .iter()
                .map(|d| crate::config::expand_tilde(d).to_string_lossy().to_string())
                .collect(),
            status: Status::Starting,
            status_detail: None,
            exit_code: None,
            last_stderr: None,
            cost_usd: 0.0,
            created_at: now,
            last_active_at: now,
        };

        {
            let record = record.clone();
            self.db.run(move |db| db.insert_agent(&record)).await?;
        }
        {
            let path = record.repo_path.clone();
            self.db.run(move |db| db.touch_repo(&path)).await.ok();
        }

        let warning = if self.running_count().await >= cfg.max_agents {
            Some(format!(
                "{} agents are already running (soft cap {}). Spawned anyway.",
                self.running_count().await,
                cfg.max_agents
            ))
        } else {
            None
        };

        self.broadcast(ServerMsg::AgentAdded {
            agent: Box::new(record.clone()),
        });

        self.launch(&record, false, req.first_message.clone())
            .await?;

        Ok(SpawnOutcome {
            agent: record,
            warning,
        })
    }

    /// Resume a stopped agent with `--resume <session_id>` (F7).
    pub async fn resume(&self, id: &str) -> Result<()> {
        if self.is_running(id).await {
            bail!("agent is already running");
        }
        let record = self.require_agent(id).await?;
        self.launch(&record, true, None).await
    }

    async fn launch(
        &self,
        record: &AgentRecord,
        resume: bool,
        first: Option<String>,
    ) -> Result<()> {
        let cfg = self.config().await;
        let work_path = PathBuf::from(&record.work_path);
        let spawn_config = SpawnConfig {
            claude_bin: cfg.claude_bin.clone(),
            cwd: work_path,
            args: LaunchArgs {
                session_id: record.id.clone(),
                resume,
                permission_mode: record.permission_mode,
                model: record.model.clone(),
                effort: record.effort.clone(),
                max_budget_usd: record.max_budget_usd,
                add_dirs: record.add_dirs.clone(),
            },
        };

        // Approvals left outstanding by an earlier process died with it: their
        // request ids mean nothing to the new child, so close them before it
        // starts rather than showing the operator a card that can never be
        // answered (§5).
        let id_for_expiry = record.id.clone();
        let expired = self
            .db
            .run(move |db| db.expire_pending_permissions(&id_for_expiry))
            .await
            .unwrap_or_default();
        for request_id in expired {
            self.broadcast(ServerMsg::PermissionResolved {
                agent_id: record.id.clone(),
                request_id,
                behavior: "expired".to_string(),
            });
        }

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        // The registry slot is claimed and the child spawned under one lock.
        // Checking `is_running` and inserting separately would let two
        // concurrent resumes both pass the check and start two children on one
        // session id.
        let mut runners = self.runners.write().await;
        if runners.contains_key(&record.id) {
            bail!("agent {} is already running", record.slug);
        }
        let (child, msgs) = match process::spawn(&spawn_config) {
            Ok(pair) => pair,
            Err(err) => {
                drop(runners);
                let text = format!("{err:#}");
                let id = record.id.clone();
                let text_for_db = text.clone();
                self.db
                    .run(move |db| {
                        db.set_status(&id, Status::Failed, None, None, Some(&text_for_db))
                    })
                    .await
                    .ok();
                self.broadcast(ServerMsg::Status {
                    agent_id: record.id.clone(),
                    status: Status::Failed,
                    status_detail: None,
                    exit_code: None,
                    last_stderr: Some(text),
                    cost_usd: record.cost_usd,
                });
                return Err(err);
            }
        };

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let commands = Arc::new(RwLock::new(Vec::new()));
        runners.insert(
            record.id.clone(),
            RunnerHandle {
                tx: cmd_tx,
                commands: commands.clone(),
                generation,
            },
        );
        drop(runners);

        let mut runner = Runner {
            id: record.id.clone(),
            db: self.db.clone(),
            bus: self.bus.clone(),
            child,
            msgs,
            cmd_rx,
            cmd_closed: false,
            status: Status::Starting,
            status_detail: None,
            cost_usd: record.cost_usd,
            pending: HashMap::new(),
            next_request_id: 1,
            commands,
            stop_requested: false,
            last_stderr: None,
            recently_sent: std::collections::VecDeque::new(),
        };

        // Status starts at Starting for both a first launch and a resume.
        runner.set_status(Transition::Spawned).await;
        // Ask for the slash command list up front (F9).
        runner.send_control(protocol::initialize_request);
        if let Some(text) = first
            && !text.trim().is_empty()
        {
            runner.send_user_message(&text).await;
        }

        let runners = self.runners_ref();
        let id = record.id.clone();
        tokio::spawn(async move {
            runner.run().await;
            deregister(&runners, &id, generation).await;
        });
        Ok(())
    }

    /// The registry is shared with each runner task so it can deregister
    /// itself when the child exits.
    fn runners_ref(&self) -> Arc<RwLock<HashMap<String, RunnerHandle>>> {
        self.runners.clone()
    }

    // -- verbs --------------------------------------------------------------

    pub async fn send_message(&self, id: &str, text: &str) -> Result<()> {
        self.command(id, AgentCommand::Send(text.to_string())).await
    }

    pub async fn decide(
        &self,
        id: &str,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        let record = self.require_agent(id).await?;
        if !record.permission_mode.intercepts_permissions() {
            bail!(
                "agent {id} runs in `{}` mode, so tool calls never reach the approval queue",
                record.permission_mode
            );
        }
        self.command(
            id,
            AgentCommand::Decide {
                request_id: request_id.to_string(),
                decision,
            },
        )
        .await
    }

    pub async fn interrupt(&self, id: &str) -> Result<()> {
        self.command(id, AgentCommand::Interrupt).await
    }

    pub async fn set_permission_mode(&self, id: &str, mode: PermissionMode) -> Result<()> {
        {
            let id = id.to_string();
            self.db
                .run(move |db| db.set_permission_mode(&id, mode))
                .await?;
        }
        // A running agent is switched live; a stopped one picks it up on resume.
        self.command(id, AgentCommand::SetPermissionMode(mode))
            .await
            .ok();
        Ok(())
    }

    pub async fn stop(&self, id: &str) -> Result<()> {
        self.command(id, AgentCommand::Stop).await
    }

    pub async fn rename(&self, id: &str, name: &str) -> Result<AgentRecord> {
        let name = name.trim().to_string();
        if name.is_empty() {
            bail!("name cannot be empty");
        }
        let agent_id = id.to_string();
        self.db
            .run(move |db| db.rename_agent(&agent_id, &name))
            .await?;
        self.require_agent(id).await
    }

    async fn command(&self, id: &str, cmd: AgentCommand) -> Result<()> {
        let runners = self.runners.read().await;
        let handle = runners
            .get(id)
            .ok_or_else(|| anyhow!("agent {id} is not running"))?;
        handle
            .tx
            .send(cmd)
            .map_err(|_| anyhow!("agent {id} is shutting down"))
    }

    async fn require_agent(&self, id: &str) -> Result<AgentRecord> {
        let agent_id = id.to_string();
        self.db
            .run(move |db| db.get_agent(&agent_id))
            .await?
            .ok_or_else(|| anyhow!("no such agent: {id}"))
    }

    // -- delete -------------------------------------------------------------

    /// Inspect what a delete would cost, without changing anything.
    pub async fn delete_preview(&self, id: &str) -> Result<git::SafetyReport> {
        let record = self.require_agent(id).await?;
        Ok(safety_for(&record).await)
    }

    /// Remove an agent, its events and (when safe) its worktree.
    ///
    /// Never commits, never pushes. The branch survives by default.
    pub async fn delete(
        &self,
        id: &str,
        force: bool,
        delete_branch: bool,
    ) -> Result<(), DeleteError> {
        let record = self
            .require_agent(id)
            .await
            .map_err(|e| DeleteError::Other(format!("{e:#}")))?;

        if self.is_running(id).await {
            self.stop(id).await.ok();
            self.await_stop(id, STOP_GRACE + Duration::from_secs(1))
                .await;
        }

        let report = safety_for(&record).await;
        if !report.safe && !force {
            let message = report
                .blocker()
                .unwrap_or_else(|| "the worktree is not clean".to_string());
            return Err(DeleteError::Unsafe(Box::new(DeleteRefusal {
                report,
                message: format!(
                    "Refusing to delete: {message}. Nothing was committed or pushed — \
                     confirm to delete anyway."
                ),
            })));
        }

        if record.uses_worktree && record.is_git {
            let repo = PathBuf::from(&record.repo_path);
            let work = PathBuf::from(&record.work_path);
            let result =
                tokio::task::spawn_blocking(move || git::remove_worktree(&repo, &work, force))
                    .await
                    .map_err(|e| DeleteError::Other(e.to_string()))?;
            if let Err(err) = result {
                if !force {
                    return Err(DeleteError::Other(format!("{err:#}")));
                }
                tracing::warn!(?err, "forced delete continued past worktree removal");
            }
        }

        if delete_branch && let (true, Some(branch)) = (record.is_git, record.branch.clone()) {
            let repo = PathBuf::from(&record.repo_path);
            let result =
                tokio::task::spawn_blocking(move || git::delete_branch(&repo, &branch, force))
                    .await
                    .map_err(|e| DeleteError::Other(e.to_string()))?;
            if let Err(err) = result {
                tracing::warn!(?err, "could not delete branch");
                self.broadcast(ServerMsg::Notice {
                    agent_id: None,
                    level: "warn".to_string(),
                    text: format!("Branch was kept: {err:#}"),
                });
            }
        }

        let agent_id = record.id.clone();
        self.db
            .run(move |db| db.delete_agent(&agent_id))
            .await
            .map_err(|e| DeleteError::Other(format!("{e:#}")))?;
        self.broadcast(ServerMsg::AgentRemoved {
            agent_id: record.id.clone(),
        });
        Ok(())
    }

    async fn await_stop(&self, id: &str, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.is_running(id).await {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// SIGTERM every child, then wait out the grace period (§4).
    pub async fn shutdown(&self) {
        let ids: Vec<String> = self.runners.read().await.keys().cloned().collect();
        for id in &ids {
            self.stop(id).await.ok();
        }
        let deadline = tokio::time::Instant::now() + STOP_GRACE + Duration::from_secs(1);
        loop {
            if self.running_count().await == 0 || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.db.run(|db| db.mark_all_stopped()).await.ok();
    }
}

/// Remove a runner's registry entry, but only if it is still that runner's.
///
/// An unconditional remove would let a doomed runner evict the handle of the
/// launch that replaced it, killing a healthy child.
async fn deregister(
    runners: &Arc<RwLock<HashMap<String, RunnerHandle>>>,
    id: &str,
    generation: u64,
) {
    let mut map = runners.write().await;
    if map.get(id).is_some_and(|h| h.generation == generation) {
        map.remove(id);
    }
}

/// Why a delete was refused.
#[derive(Debug)]
pub enum DeleteError {
    Unsafe(Box<DeleteRefusal>),
    Other(String),
}

impl std::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeleteError::Unsafe(r) => f.write_str(&r.message),
            DeleteError::Other(msg) => f.write_str(msg),
        }
    }
}

/// The safety check, with every failure mode folded into an unsafe report.
///
/// A check that could not run must never read as "nothing would be lost".
async fn safety_for(record: &AgentRecord) -> git::SafetyReport {
    if !record.is_git {
        return git::SafetyReport {
            safe: true,
            branch_empty_or_merged: true,
            ..Default::default()
        };
    }
    let work = PathBuf::from(&record.work_path);
    let repo = PathBuf::from(&record.repo_path);
    let branch = record.branch.clone();
    let base = record.base_ref.clone();
    match tokio::task::spawn_blocking(move || {
        git::safety_report(&work, &repo, branch.as_deref(), base.as_deref())
    })
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => git::SafetyReport::failed(format!("{err:#}")),
        Err(err) => git::SafetyReport::failed(err),
    }
}

/// The workspace an agent will run in.
struct Prepared {
    slug: String,
    branch: Option<String>,
    base_ref: Option<String>,
    work_path: PathBuf,
    is_git: bool,
    uses_worktree: bool,
}

/// Blocking: allocate names and create the worktree or in-place branch.
fn prepare_workspace(
    repo_path: &Path,
    task_name: &str,
    prefix: &str,
    taken_slugs: &HashSet<String>,
    req: &SpawnRequest,
) -> Result<Prepared> {
    let is_git = git::is_git_repo(repo_path);
    if !is_git {
        // Non-git folders spawn normally, with no branch. Never `git init` (§6).
        let slug = git::unique_name(&git::slugify(task_name), |c| taken_slugs.contains(c));
        return Ok(Prepared {
            slug,
            branch: None,
            base_ref: None,
            work_path: repo_path.to_path_buf(),
            is_git: false,
            uses_worktree: false,
        });
    }

    let existing: HashSet<String> = git::list_branches(repo_path).into_iter().collect();
    let (slug, branch) = git::allocate_names(task_name, prefix, taken_slugs, &existing);
    let base_ref = req
        .base_ref
        .clone()
        .filter(|b| !b.trim().is_empty())
        .or_else(|| git::current_branch(repo_path))
        .unwrap_or_else(|| "HEAD".to_string());
    // Refuse an option-shaped base ref before anything is created or stored;
    // it would otherwise be replayed by every later delete preview.
    git::validate_ref(&base_ref).with_context(|| format!("base ref `{base_ref}`"))?;

    if req.in_place {
        git::create_branch_in_place(repo_path, &branch, Some(&base_ref))?;
        return Ok(Prepared {
            slug,
            branch: Some(branch),
            base_ref: Some(base_ref),
            work_path: repo_path.to_path_buf(),
            is_git: true,
            uses_worktree: false,
        });
    }

    let root = repo_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("repository has no parent directory"))?;
    let repo_name = repo_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let work_path = git::worktree_path(&root, &repo_name, &slug);
    git::add_worktree(repo_path, &work_path, &branch, Some(&base_ref))?;

    Ok(Prepared {
        slug,
        branch: Some(branch),
        base_ref: Some(base_ref),
        work_path,
        is_git: true,
        uses_worktree: true,
    })
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct Runner {
    id: String,
    db: Db,
    bus: broadcast::Sender<ServerMsg>,
    child: ChildHandle,
    msgs: mpsc::UnboundedReceiver<ProcessMsg>,
    cmd_rx: mpsc::UnboundedReceiver<AgentCommand>,
    /// Set once the command channel is closed. A closed `UnboundedReceiver`
    /// returns `None` immediately and forever, so it must stop being polled or
    /// the select loop spins.
    cmd_closed: bool,
    status: Status,
    /// The live sub-label naming the current tool (§4). Held here so an
    /// unrelated status transition does not wipe it.
    status_detail: Option<String>,
    cost_usd: f64,
    pending: HashMap<String, PermissionRequest>,
    next_request_id: u64,
    commands: Arc<RwLock<Vec<SlashCommand>>>,
    stop_requested: bool,
    last_stderr: Option<String>,
    /// Messages we wrote to stdin and already persisted. The CLI echoes user
    /// lines back on stdout; without this the transcript shows them twice.
    recently_sent: std::collections::VecDeque<String>,
}

impl Runner {
    async fn run(mut self) {
        loop {
            let listening = !self.cmd_closed;
            tokio::select! {
                msg = self.msgs.recv() => match msg {
                    Some(ProcessMsg::Action(action)) => self.on_action(action).await,
                    Some(ProcessMsg::Exited(info)) => {
                        self.on_exit(info).await;
                        break;
                    }
                    None => {
                        self.on_exit(ExitInfo { code: None, signal: None, requested: self.stop_requested }).await;
                        break;
                    }
                },
                cmd = self.cmd_rx.recv(), if listening => match cmd {
                    Some(cmd) => self.on_command(cmd).await,
                    // Every sender is gone; the supervisor is going away. Ask
                    // the child to stop once, then stop polling this channel —
                    // it would return `None` forever — and drain `msgs` until
                    // the exit arrives.
                    None => {
                        self.cmd_closed = true;
                        self.stop_requested = true;
                        self.child.stop(STOP_GRACE);
                    }
                },
            }
        }
    }

    fn emit(&self, msg: ServerMsg) {
        let _ = self.bus.send(msg);
    }

    async fn persist(&self, kind: EventKind, payload: Value) {
        let id = self.id.clone();
        let payload_for_db = payload.clone();
        match self
            .db
            .run(move |db| db.append_event(&id, kind, &payload_for_db))
            .await
        {
            Ok(seq) => self.emit(ServerMsg::Event {
                agent_id: self.id.clone(),
                seq,
                ts: now_ms(),
                kind: kind.as_str().to_string(),
                payload,
            }),
            Err(err) => tracing::error!(?err, agent = %self.id, "failed to persist event"),
        }
    }

    async fn set_status(&mut self, transition: Transition) {
        let Some(next) = self.status.apply(transition) else {
            tracing::debug!(agent = %self.id, ?transition, status = %self.status, "ignored transition");
            return;
        };
        if transition == Transition::Spawned {
            self.status_detail = None;
        }
        self.status = next;
        self.publish_status().await;
    }

    /// Set the `Working` sub-label. `None` clears it — which only the end of a
    /// turn does.
    async fn set_detail(&mut self, detail: Option<String>) {
        self.status_detail = detail.clone();
        let id = self.id.clone();
        self.db
            .run(move |db| db.set_status_detail(&id, detail.as_deref()))
            .await
            .ok();
        self.emit(ServerMsg::Status {
            agent_id: self.id.clone(),
            status: self.status,
            status_detail: self.status_detail.clone(),
            exit_code: None,
            last_stderr: self.last_stderr.clone(),
            cost_usd: self.cost_usd,
        });
    }

    /// Write and announce the current status, carrying the sub-label with it.
    async fn publish_status(&self) {
        let id = self.id.clone();
        let status = self.status;
        let stderr = self.last_stderr.clone();
        let detail = self.status_detail.clone();
        let detail_for_db = detail.clone();
        let stderr_for_db = stderr.clone();
        self.db
            .run(move |db| {
                db.set_status(
                    &id,
                    status,
                    detail_for_db.as_deref(),
                    None,
                    stderr_for_db.as_deref(),
                )
            })
            .await
            .ok();
        self.emit(ServerMsg::Status {
            agent_id: self.id.clone(),
            status: self.status,
            status_detail: detail,
            exit_code: None,
            last_stderr: stderr,
            cost_usd: self.cost_usd,
        });
    }

    async fn on_action(&mut self, action: Action) {
        match action {
            Action::Persist { kind, payload } => {
                if kind == EventKind::User && self.is_echo(&payload) {
                    return;
                }
                if kind == EventKind::Stderr {
                    self.last_stderr = payload
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                self.persist(kind, payload).await;
            }
            Action::Partial(payload) => self.emit(ServerMsg::Partial {
                agent_id: self.id.clone(),
                payload,
            }),
            Action::Transition(t) => self.set_status(t).await,
            Action::StatusDetail(detail) => self.set_detail(detail).await,
            Action::Cost(cost) => {
                self.cost_usd = cost;
                let id = self.id.clone();
                self.db.run(move |db| db.set_cost(&id, cost)).await.ok();
            }
            Action::Permission(request) => {
                self.pending
                    .insert(request.request_id.clone(), (*request).clone());
                self.emit(ServerMsg::PermissionRequest {
                    agent_id: self.id.clone(),
                    request: *request,
                });
            }
            Action::ControlResponse {
                request_id,
                payload,
                is_error,
            } => {
                if is_error {
                    tracing::warn!(agent = %self.id, %request_id, ?payload, "control request failed");
                }
                if payload.get("still_queued").is_some() {
                    self.emit(ServerMsg::Queued {
                        agent_id: self.id.clone(),
                        still_queued: payload["still_queued"].clone(),
                    });
                }
            }
            Action::Commands(commands) => {
                *self.commands.write().await = commands.clone();
                self.emit(ServerMsg::Commands {
                    agent_id: self.id.clone(),
                    commands,
                });
            }
            Action::SessionId(_) => {}
            Action::Unrecognised { kind, reason } => {
                tracing::warn!(agent = %self.id, %kind, %reason, "unrecognised protocol event");
                self.emit(ServerMsg::Notice {
                    agent_id: Some(self.id.clone()),
                    level: "warn".to_string(),
                    text: format!("Unrecognised protocol event `{kind}`: {reason}"),
                });
            }
        }
    }

    async fn on_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::Send(text) => self.send_user_message(&text).await,
            AgentCommand::Decide {
                request_id,
                decision,
            } => self.decide(&request_id, decision).await,
            AgentCommand::Interrupt => {
                self.send_control(protocol::interrupt_request);
            }
            AgentCommand::SetPermissionMode(mode) => {
                if let Some(value) = mode.control_value() {
                    let id = self.take_request_id();
                    let request = protocol::set_permission_mode_request(&id, value);
                    self.write(request);
                }
            }
            AgentCommand::Stop => {
                self.stop_requested = true;
                self.child.stop(STOP_GRACE);
            }
        }
    }

    /// Is this `user` line the CLI repeating something we just sent?
    fn is_echo(&mut self, payload: &Value) -> bool {
        let Ok(msg) = serde_json::from_value::<protocol::MessageLine>(payload.clone()) else {
            return false;
        };
        let text = msg.text();
        if text.is_empty() {
            return false;
        }
        if let Some(pos) = self.recently_sent.iter().position(|t| *t == text) {
            self.recently_sent.remove(pos);
            return true;
        }
        false
    }

    async fn send_user_message(&mut self, text: &str) {
        self.recently_sent.push_back(text.to_string());
        if self.recently_sent.len() > 32 {
            self.recently_sent.pop_front();
        }
        self.write(protocol::user_message(text));
        self.persist(
            EventKind::User,
            json!({"type": "user", "message": {"role": "user", "content": text}}),
        )
        .await;
        self.set_status(Transition::TurnStarted).await;
    }

    async fn decide(&mut self, request_id: &str, decision: PermissionDecision) {
        let original = self
            .pending
            .remove(request_id)
            .map(|r| r.input)
            .unwrap_or(Value::Null);
        self.write(protocol::permission_response(
            request_id, &decision, &original,
        ));
        self.persist(
            EventKind::PermissionDecision,
            json!({
                "request_id": request_id,
                "behavior": decision.behavior(),
            }),
        )
        .await;
        self.emit(ServerMsg::PermissionResolved {
            agent_id: self.id.clone(),
            request_id: request_id.to_string(),
            behavior: decision.behavior().to_string(),
        });
        self.set_status(Transition::PermissionResolved).await;
    }

    fn take_request_id(&mut self) -> String {
        let id = format!("req_{}", self.next_request_id);
        self.next_request_id += 1;
        id
    }

    fn send_control(&mut self, build: fn(&str) -> Value) {
        let id = self.take_request_id();
        let value = build(&id);
        self.write(value);
    }

    fn write(&self, value: Value) {
        if let Err(err) = self.child.send(value) {
            tracing::warn!(?err, agent = %self.id, "could not write to child");
        }
    }

    async fn on_exit(&mut self, info: ExitInfo) {
        let requested = info.requested || self.stop_requested;
        let (transition, fallback) = if requested {
            (Transition::Exited, Status::Stopped)
        } else {
            (Transition::Errored, Status::Failed)
        };
        let status = self.status.apply(transition).unwrap_or(fallback);
        self.status = status;
        let code = info.code.map(i64::from);
        let stderr = self.last_stderr.clone();
        let detail = info
            .signal
            .map(|s| format!("terminated by signal {s}"))
            .or_else(|| info.code.map(|c| format!("exited with code {c}")));
        self.status_detail = detail.clone();

        // Whatever this process was still asking permission for can never be
        // answered now: close it out so the next launch starts clean (§5).
        let outstanding: Vec<String> = self.pending.keys().cloned().collect();
        for request_id in outstanding {
            let tool = self
                .pending
                .remove(&request_id)
                .map(|r| r.tool_name)
                .unwrap_or_default();
            self.persist(
                EventKind::PermissionDecision,
                json!({
                    "request_id": request_id,
                    "behavior": "expired",
                    "tool_name": tool,
                }),
            )
            .await;
            self.emit(ServerMsg::PermissionResolved {
                agent_id: self.id.clone(),
                request_id,
                behavior: "expired".to_string(),
            });
        }

        let id = self.id.clone();
        let detail_for_db = detail.clone();
        let stderr_for_db = stderr.clone();
        self.db
            .run(move |db| {
                db.set_status(
                    &id,
                    status,
                    detail_for_db.as_deref(),
                    code,
                    stderr_for_db.as_deref(),
                )
            })
            .await
            .ok();

        self.persist(
            EventKind::System,
            json!({
                "type": "system",
                "subtype": "process_exit",
                "code": info.code,
                "signal": info.signal,
                "requested": requested,
            }),
        )
        .await;

        self.emit(ServerMsg::Status {
            agent_id: self.id.clone(),
            status,
            status_detail: detail,
            exit_code: code,
            last_stderr: stderr,
            cost_usd: self.cost_usd,
        });
        // No auto-restart: an unexpected exit waits for a human (§4).
        if !requested {
            tracing::warn!(agent = %self.id, ?info, "agent exited unexpectedly");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::process::ChildHandle;
    use crate::db::now_ms;
    use tokio::sync::broadcast::Receiver;

    fn agent_record(id: &str, work_path: &Path) -> AgentRecord {
        AgentRecord {
            id: id.to_string(),
            name: "Test agent".to_string(),
            slug: format!("slug_{id}"),
            repo_path: work_path.to_string_lossy().to_string(),
            work_path: work_path.to_string_lossy().to_string(),
            is_git: false,
            branch: None,
            base_ref: None,
            uses_worktree: false,
            permission_mode: PermissionMode::Ask,
            model: None,
            effort: None,
            max_budget_usd: None,
            add_dirs: Vec::new(),
            status: Status::Stopped,
            status_detail: None,
            exit_code: None,
            last_stderr: None,
            cost_usd: 0.0,
            created_at: now_ms(),
            last_active_at: now_ms(),
        }
    }

    /// A runner wired to a child that does not exist, so the supervision logic
    /// can be driven directly from synthetic process messages.
    struct Harness {
        db: Db,
        id: String,
        msgs: mpsc::UnboundedSender<ProcessMsg>,
        cmds: Option<mpsc::UnboundedSender<AgentCommand>>,
        events: Receiver<ServerMsg>,
        stops: Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
        _stdin: mpsc::UnboundedReceiver<Value>,
    }

    impl Harness {
        fn start() -> Self {
            let db = Db::open_in_memory().expect("db");
            let dir = std::env::temp_dir();
            let record = agent_record("agent-1", &dir);
            db.insert_agent(&record).expect("insert");

            let (bus, events) = broadcast::channel(256);
            let (child, stdin, stops) = ChildHandle::detached();
            let (msg_tx, msg_rx) = mpsc::unbounded_channel();
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

            let runner = Runner {
                id: record.id.clone(),
                db: db.clone(),
                bus,
                child,
                msgs: msg_rx,
                cmd_rx,
                cmd_closed: false,
                status: Status::Starting,
                status_detail: None,
                cost_usd: 0.0,
                pending: HashMap::new(),
                next_request_id: 1,
                commands: Arc::new(RwLock::new(Vec::new())),
                stop_requested: false,
                last_stderr: None,
                recently_sent: std::collections::VecDeque::new(),
            };

            Self {
                db,
                id: record.id,
                msgs: msg_tx,
                cmds: Some(cmd_tx),
                events,
                stops,
                task: tokio::spawn(runner.run()),
                _stdin: stdin,
            }
        }

        fn action(&self, action: Action) {
            self.msgs
                .send(ProcessMsg::Action(action))
                .expect("runner is alive");
        }

        /// Wait for the next status broadcast — the runner's own acknowledgement
        /// that it processed what we sent, so no test needs a sleep.
        async fn next_status(&mut self) -> (Status, Option<String>) {
            loop {
                match self.events.recv().await.expect("bus is alive") {
                    ServerMsg::Status {
                        status,
                        status_detail,
                        ..
                    } => return (status, status_detail),
                    _ => continue,
                }
            }
        }

        async fn finish(mut self) -> Db {
            self.msgs
                .send(ProcessMsg::Exited(ExitInfo {
                    code: Some(0),
                    signal: None,
                    requested: true,
                }))
                .ok();
            self.task.await.expect("runner should finish");
            self.cmds.take();
            self.db
        }
    }

    // -- the command channel closing must end the loop, not spin on it -------

    #[tokio::test]
    async fn a_closed_command_channel_stops_the_child_once_and_terminates() {
        let mut harness = Harness::start();
        let cmds = harness.cmds.take().expect("sender");
        drop(cmds);

        // Give the runner every chance to spin. A loop that keeps polling a
        // closed receiver would ask the child to stop on every iteration.
        for _ in 0..500 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            harness.stops.load(std::sync::atomic::Ordering::Acquire),
            1,
            "a closed command channel must ask for a stop exactly once"
        );

        // And the loop is still able to finish when the child exits.
        harness
            .msgs
            .send(ProcessMsg::Exited(ExitInfo {
                code: Some(143),
                signal: Some(15),
                requested: true,
            }))
            .expect("runner is alive");
        harness.task.await.expect("runner should terminate");
    }

    // -- the Working sub-label survives unrelated transitions ----------------

    #[tokio::test]
    async fn status_detail_survives_an_unrelated_transition() {
        let mut harness = Harness::start();

        harness.action(Action::StatusDetail(Some("Bash: cargo test".to_string())));
        let (_, detail) = harness.next_status().await;
        assert_eq!(detail.as_deref(), Some("Bash: cargo test"));

        // Asking for permission must not wipe the label naming the tool.
        harness.action(Action::Transition(Transition::PermissionRequested));
        let (status, detail) = harness.next_status().await;
        assert_eq!(status, Status::AwaitingApproval);
        assert_eq!(
            detail.as_deref(),
            Some("Bash: cargo test"),
            "the sub-label must survive the transition"
        );

        let id = harness.id.clone();
        let db = harness.finish().await;
        let stored = db.get_agent(&id).expect("get").expect("present");
        assert_eq!(stored.status, Status::Stopped);

        // A turn ending is the one thing that clears it.
        let mut harness = Harness::start();
        harness.action(Action::StatusDetail(Some("Read: /a".to_string())));
        harness.next_status().await;
        harness.action(Action::StatusDetail(None));
        let (_, detail) = harness.next_status().await;
        assert_eq!(detail, None);
        harness.finish().await;
    }

    // -- approvals do not outlive the process that asked --------------------

    #[tokio::test]
    async fn pending_approvals_do_not_survive_the_process_that_asked() {
        let mut harness = Harness::start();
        harness.action(Action::Permission(Box::new(PermissionRequest {
            request_id: "77".to_string(),
            tool_name: "Write".to_string(),
            display_name: None,
            description: None,
            tool_use_id: None,
            input: json!({"file_path": "/tmp/x"}),
            permission_suggestions: Value::Null,
        })));
        harness.action(Action::Persist {
            kind: EventKind::PermissionRequest,
            payload: json!({"request_id": "77", "tool_name": "Write", "input": {}}),
        });
        harness.action(Action::Transition(Transition::PermissionRequested));
        let (status, _) = harness.next_status().await;
        assert_eq!(status, Status::AwaitingApproval);

        let id = harness.id.clone();
        let db = harness.finish().await;

        let pending = db.pending_permissions(&id).expect("pending");
        assert!(
            pending.is_empty(),
            "an approval the dead process asked for can never be answered: {pending:?}"
        );
        let decisions: Vec<_> = db
            .events_after(&id, 0, 100)
            .expect("events")
            .into_iter()
            .filter(|e| e.kind == "permission_decision")
            .collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].payload["behavior"], json!("expired"));
    }

    #[tokio::test]
    async fn a_relaunch_clears_approvals_left_over_from_a_crashed_process() {
        let db = Db::open_in_memory().expect("db");
        let dir = tempfile::tempdir().expect("tempdir");
        let record = agent_record("agent-2", dir.path());
        db.insert_agent(&record).expect("insert");
        // As if the server had died mid-approval, before on_exit could run.
        db.append_event(
            &record.id,
            EventKind::PermissionRequest,
            &json!({"request_id": "9", "tool_name": "Bash", "input": {}}),
        )
        .expect("append");
        assert_eq!(
            db.pending_permissions(&record.id).expect("pending").len(),
            1
        );

        let sup = Supervisor::new(db.clone(), Arc::new(RwLock::new(missing_binary_config())));
        // The launch itself fails (there is no CLI), but the stale approval is
        // cleared before the child would ever have started.
        sup.resume(&record.id).await.expect_err("no such binary");
        assert!(
            db.pending_permissions(&record.id)
                .expect("pending")
                .is_empty()
        );
    }

    fn missing_binary_config() -> Config {
        Config {
            claude_bin: "/nonexistent/claude-web-test-binary".to_string(),
            ..Config::default()
        }
    }

    // -- one child per agent, whatever the interleaving ----------------------

    /// A stand-in CLI that ignores its arguments and stays alive until its
    /// stdin closes or it is signalled. No `claude`, no network, no timers.
    fn stub_cli(dir: &Path) -> Option<String> {
        let path = dir.join("stub-cli");
        std::fs::write(&path, "#!/bin/sh\nexec cat >/dev/null\n").ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        Some(path.to_string_lossy().to_string())
    }

    #[tokio::test]
    async fn two_concurrent_resumes_start_exactly_one_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(bin) = stub_cli(dir.path()) else {
            return;
        };
        let db = Db::open_in_memory().expect("db");
        let record = agent_record("agent-3", dir.path());
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(
            db.clone(),
            Arc::new(RwLock::new(Config {
                claude_bin: bin,
                ..Config::default()
            })),
        );

        // A double-click on Resume: both requests are in flight at once.
        let (first, second) = tokio::join!(sup.resume(&record.id), sup.resume(&record.id));
        assert!(
            first.is_ok() ^ second.is_ok(),
            "exactly one resume may win: {first:?} / {second:?}"
        );
        let loser = first.err().or(second.err()).expect("one must lose");
        assert!(
            format!("{loser:#}").contains("already running"),
            "{loser:#}"
        );
        assert_eq!(sup.running_count().await, 1, "one agent, one child");
        assert!(
            sup.is_running(&record.id).await,
            "the winner must still be registered"
        );

        sup.shutdown().await;
    }

    #[tokio::test]
    async fn a_stale_runner_cannot_deregister_the_launch_that_replaced_it() {
        let runners: Arc<RwLock<HashMap<String, RunnerHandle>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        runners.write().await.insert(
            "a".to_string(),
            RunnerHandle {
                tx,
                commands: Arc::new(RwLock::new(Vec::new())),
                generation: 2,
            },
        );

        // The older, doomed runner finishes and tries to clean up.
        deregister(&runners, "a", 1).await;
        assert!(
            runners.read().await.contains_key("a"),
            "generation 1 must not evict generation 2"
        );

        // The current runner's own cleanup does remove it.
        deregister(&runners, "a", 2).await;
        assert!(!runners.read().await.contains_key("a"));
    }

    #[tokio::test]
    async fn a_failed_launch_leaves_no_registry_entry_and_marks_the_agent_failed() {
        let db = Db::open_in_memory().expect("db");
        let dir = tempfile::tempdir().expect("tempdir");
        let record = agent_record("agent-4", dir.path());
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(db.clone(), Arc::new(RwLock::new(missing_binary_config())));

        sup.resume(&record.id).await.expect_err("no such binary");
        assert_eq!(sup.running_count().await, 0);
        let stored = db.get_agent(&record.id).expect("get").expect("present");
        assert_eq!(stored.status, Status::Failed);
        assert!(stored.last_stderr.is_some());
    }

    #[tokio::test]
    async fn add_dirs_are_persisted_so_a_resume_keeps_them() {
        let db = Db::open_in_memory().expect("db");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut record = agent_record("agent-5", dir.path());
        record.add_dirs = vec!["/extra/one".to_string(), "/extra/two".to_string()];
        db.insert_agent(&record).expect("insert");

        // Reading the agent back is what a resume does, and the launch argv is
        // built from exactly that.
        let stored = db.get_agent(&record.id).expect("get").expect("present");
        let argv = LaunchArgs {
            session_id: stored.id.clone(),
            resume: true,
            permission_mode: stored.permission_mode,
            model: stored.model.clone(),
            effort: stored.effort.clone(),
            max_budget_usd: stored.max_budget_usd,
            add_dirs: stored.add_dirs.clone(),
        }
        .to_argv();
        assert_eq!(argv.iter().filter(|a| *a == "--add-dir").count(), 2);
        assert!(argv.iter().any(|a| a == "/extra/two"));
    }

    #[tokio::test]
    async fn a_non_intercepting_agent_cannot_be_asked_for_a_decision() {
        let db = Db::open_in_memory().expect("db");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut record = agent_record("agent-6", dir.path());
        record.permission_mode = PermissionMode::Bypass;
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(db, Arc::new(RwLock::new(Config::default())));

        let err = sup
            .decide(
                &record.id,
                "1",
                PermissionDecision::Allow {
                    updated_input: None,
                },
            )
            .await
            .expect_err("bypass never reaches the approval queue");
        assert!(format!("{err:#}").contains("bypass"), "{err:#}");
    }
}
