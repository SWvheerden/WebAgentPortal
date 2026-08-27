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

use super::process::{self, Action, ChildHandle, ExitInfo, ProcessMsg, SpawnConfig, Sweep};
use super::protocol::{
    self, EventKind, LaunchArgs, PermissionDecision, PermissionRequest, RateLimitInfo, SlashCommand,
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
    /// Account usage against the rate-limit windows. Account-wide rather than
    /// per-agent: whichever agent's CLI reported it last is speaking for all of
    /// them, so this carries no `agent_id`.
    RateLimit {
        info: Box<RateLimitInfo>,
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
    /// Check out this existing branch instead of creating a new one (§6). The
    /// name must already be a local branch; anything else is refused rather
    /// than created.
    #[serde(default)]
    pub existing_branch: Option<String>,
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
    /// The pids of every live agent CLI. Each is its own group leader, and no
    /// agent may ever sweep another's group.
    agent_pids: Arc<RwLock<HashSet<i32>>>,
    next_generation: AtomicU64,
    /// The last rate-limit snapshot any agent's CLI reported. Kept so a browser
    /// that connects between two events still sees the numbers, rather than a
    /// blank panel until the next API call.
    rate_limit: Arc<RwLock<Option<RateLimitInfo>>>,
    bus: broadcast::Sender<ServerMsg>,
}

impl Supervisor {
    pub fn new(db: Db, config: Arc<RwLock<Config>>) -> Arc<Self> {
        let (bus, _) = broadcast::channel(2048);
        Arc::new(Self {
            db,
            config,
            runners: Arc::new(RwLock::new(HashMap::new())),
            agent_pids: Arc::new(RwLock::new(HashSet::new())),
            next_generation: AtomicU64::new(1),
            rate_limit: Arc::new(RwLock::new(None)),
            bus,
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The last rate-limit snapshot, for a freshly loaded page.
    pub async fn rate_limit(&self) -> Option<RateLimitInfo> {
        self.rate_limit.read().await.clone()
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
        if let Some(model) = req.model.as_deref().filter(|m| !m.trim().is_empty()) {
            protocol::validate_cli_value("model", model)?;
        }
        if let Some(effort) = req.effort.as_deref().filter(|e| !e.trim().is_empty()) {
            protocol::validate_cli_value("effort", effort)?;
        }

        // The repository, and every extra directory the agent is given, must be
        // inside the configured roots. Otherwise a spawn is an arbitrary-path
        // primitive: a non-git directory becomes the work path verbatim, and
        // `--add-dir /` hands the agent the whole filesystem.
        let roots = cfg.roots();
        let requested = crate::config::expand_tilde(&req.repo_path);
        let extra: Vec<PathBuf> = req
            .add_dirs
            .iter()
            .filter(|d| !d.trim().is_empty())
            .map(|d| crate::config::expand_tilde(d))
            .collect();
        let roots_for_check = roots.clone();
        let (repo_path, add_dirs) = tokio::task::spawn_blocking(move || -> Result<_> {
            let repo = crate::config::confine_to_roots(&requested, &roots_for_check)
                .context("the repository")?;
            if !repo.is_dir() {
                bail!("{} is not a directory", repo.display());
            }
            let mut dirs = Vec::with_capacity(extra.len());
            for dir in extra {
                dirs.push(
                    crate::config::confine_to_roots(&dir, &roots_for_check)
                        .with_context(|| format!("extra directory {}", dir.display()))?
                        .to_string_lossy()
                        .to_string(),
                );
            }
            Ok((repo, dirs))
        })
        .await
        .context("path check panicked")??;

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
            branch_is_new: prepared.branch_is_new,
            permission_mode: req.permission_mode.unwrap_or(cfg.default_permission_mode),
            model: req
                .model
                .clone()
                .or_else(|| Some(cfg.default_model.clone())),
            effort: req.effort.clone(),
            max_budget_usd: req.max_budget_usd,
            add_dirs,
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

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let commands = Arc::new(RwLock::new(Vec::new()));

        // Claim the registry slot before anything else happens. Checking
        // `is_running` and inserting separately would let two concurrent
        // resumes both pass the check and start two children on one session id;
        // claiming first also means nothing below this line runs twice, so a
        // losing resume changes no state at all before it bails.
        {
            let mut runners = self.runners.write().await;
            if runners.contains_key(&record.id) {
                bail!("agent {} is already running", record.slug);
            }
            runners.insert(
                record.id.clone(),
                RunnerHandle {
                    tx: cmd_tx,
                    commands: commands.clone(),
                    generation,
                },
            );
        }

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

        let (child, msgs) = match process::spawn(&spawn_config) {
            Ok(pair) => pair,
            Err(err) => {
                // Give the slot back, or the agent could never be launched again.
                deregister(&self.runners, &record.id, generation).await;
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

        self.agent_pids.write().await.insert(child.pid as i32);

        let mut runner = Runner {
            id: record.id.clone(),
            agent_pids: self.agent_pids.clone(),
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
            outstanding: HashSet::new(),
            init_request_id: None,
            commands,
            stop_requested: false,
            last_stderr: None,
            sweep: Arc::new(RwLock::new(Sweep::default())),
            last_refresh: std::time::Instant::now(),
            grace: STOP_GRACE,
            recently_sent: std::collections::VecDeque::new(),
            rate_limit: self.rate_limit.clone(),
        };

        // Status starts at Starting for both a first launch and a resume.
        runner.set_status(Transition::Spawned).await;
        // Ask for the slash command list up front (F9). The answer doubles as
        // the readiness signal that moves the agent to `Idle`, so remember
        // which request it is.
        let init_id = runner.send_control(protocol::initialize_request);
        runner.init_request_id = Some(init_id);
        if let Some(text) = first
            && !text.trim().is_empty()
        {
            runner.send_user_message(&text).await;
        }

        let runners = self.runners_ref();
        let id = record.id.clone();
        let agent_pids = self.agent_pids.clone();
        let child_pid = runner.child.pid as i32;
        tokio::spawn(async move {
            runner.run().await;
            agent_pids.write().await.remove(&child_pid);
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

    /// Change an agent's permission mode.
    ///
    /// Relaxing it is a security decision: it must be confirmed explicitly, and
    /// it is written into the agent's own event log with who asked for it, so a
    /// switch is visible in the transcript rather than silent.
    pub async fn set_permission_mode(
        &self,
        id: &str,
        mode: PermissionMode,
        confirmed: bool,
    ) -> Result<()> {
        let record = self.require_agent(id).await?;
        let current = record.permission_mode;
        if mode == current {
            return Ok(());
        }
        if mode.relaxes(current) && !confirmed {
            bail!(
                "switching from `{current}` to `{mode}` gives the agent more freedom; \
                 confirm the change explicitly"
            );
        }
        let running = self.is_running(id).await;
        if running && mode.control_value().is_none() {
            // `--dangerously-skip-permissions` is a launch flag with no runtime
            // equivalent. Recording it while sending nothing would leave the
            // displayed mode diverging from the one in force, and every prompt
            // the child kept asking would then be refused.
            bail!(
                "`{mode}` can only be applied at launch: stop the agent and resume it in that mode"
            );
        }

        {
            let id = id.to_string();
            self.db
                .run(move |db| db.set_permission_mode(&id, mode))
                .await?;
        }
        let agent_id = id.to_string();
        let payload = json!({
            "type": "system",
            "subtype": "permission_mode_change",
            "from": current.as_str(),
            "to": mode.as_str(),
            // Only the operator can reach this: the endpoint requires the
            // session token, which agents never see.
            "initiator": "operator",
            "relaxed": mode.relaxes(current),
        });
        let payload_for_db = payload.clone();
        if let Ok(seq) = self
            .db
            .run(move |db| db.append_event(&agent_id, EventKind::System, &payload_for_db))
            .await
        {
            self.broadcast(ServerMsg::Event {
                agent_id: id.to_string(),
                seq,
                ts: now_ms(),
                kind: EventKind::System.as_str().to_string(),
                payload,
            });
        }
        tracing::info!(agent = %id, %current, %mode, "operator changed the permission mode");

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
            self.await_stop(id, Self::teardown_deadline()).await;
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
                // git's own stderr is the useful part here — typically
                // "contains modified or untracked files" or a process still
                // holding the checkout — so it is passed through verbatim
                // rather than flattened into a generic failure.
                if !force {
                    return Err(DeleteError::Other(format!("{err:#}")));
                }
                tracing::warn!(?err, "forced delete continued past worktree removal");
                self.broadcast(ServerMsg::Notice {
                    agent_id: Some(record.id.clone()),
                    level: "warn".to_string(),
                    text: format!(
                        "The agent was deleted, but its worktree at {} could not be removed \
                         cleanly: {err:#}",
                        record.work_path
                    ),
                });
            }
        }

        // A branch we did not create is not ours to delete, whatever the
        // request or the force flag says.
        if delete_branch && record.is_git && !record.branch_is_new {
            self.broadcast(ServerMsg::Notice {
                agent_id: None,
                level: "warn".to_string(),
                text: format!(
                    "Branch was kept: {} existed before this agent, so deleting the agent \
                     does not delete it.",
                    record.branch.as_deref().unwrap_or("the branch")
                ),
            });
        } else if delete_branch && let (true, Some(branch)) = (record.is_git, record.branch.clone())
        {
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

    /// How long a caller may have to wait for an agent to finish dying: the
    /// child's own SIGTERM→SIGKILL grace, then the runner's teardown of any
    /// process groups its tool calls left running.
    pub fn teardown_deadline() -> Duration {
        STOP_GRACE * 2 + Duration::from_secs(1)
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
        // Wait for every runner to finish, not merely for the children to die:
        // a runner is still tearing down the process groups its tool calls left
        // running, and returning here would drop the runtime and cancel that.
        let deadline = tokio::time::Instant::now() + Self::teardown_deadline();
        loop {
            if self.running_count().await == 0 || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if self.running_count().await > 0 {
            tracing::warn!("shutting down with agents still finishing their teardown");
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
#[derive(Debug)]
struct Prepared {
    slug: String,
    branch: Option<String>,
    base_ref: Option<String>,
    work_path: PathBuf,
    is_git: bool,
    uses_worktree: bool,
    branch_is_new: bool,
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
            branch_is_new: false,
        });
    }

    let existing: HashSet<String> = git::list_branches(repo_path).into_iter().collect();
    let reused = req
        .existing_branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());

    // Reusing a branch and naming a base ref are contradictory: the branch
    // already has a head, and a start point would move it. The base is dropped
    // rather than silently applied, and stored as None so the delete-time
    // safety check reports the branch's whole unpushed history — none of which
    // this agent created.
    let (branch, base_ref, branch_is_new) = match reused {
        Some(name) => {
            // Membership, not just shape: a name that is not already a branch
            // must never reach git, or "reuse" would quietly create one.
            if !existing.contains(name) {
                bail!("`{name}` is not a branch of this repository");
            }
            (name.to_string(), None, false)
        }
        None => {
            let base = req
                .base_ref
                .clone()
                .filter(|b| !b.trim().is_empty())
                .or_else(|| git::current_branch(repo_path))
                .unwrap_or_else(|| "HEAD".to_string());
            // Refuse an option-shaped base ref before anything is created or
            // stored; it would otherwise be replayed by every later delete
            // preview.
            git::validate_ref(&base).with_context(|| format!("base ref `{base}`"))?;
            (String::new(), Some(base), true)
        }
    };

    // The slug names the agent and its worktree directory, so it is allocated
    // from the task name either way — only a *new* branch takes its name from it.
    let (slug, branch) = if branch_is_new {
        git::allocate_names(task_name, prefix, taken_slugs, &existing)
    } else {
        let slug = git::unique_name(&git::slugify(task_name), |c| taken_slugs.contains(c));
        (slug, branch)
    };

    if req.in_place {
        if branch_is_new {
            git::create_branch_in_place(repo_path, &branch, base_ref.as_deref())?;
        } else {
            git::checkout_existing_branch(repo_path, &branch)?;
        }
        return Ok(Prepared {
            slug,
            branch: Some(branch),
            base_ref,
            work_path: repo_path.to_path_buf(),
            is_git: true,
            uses_worktree: false,
            branch_is_new,
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
    if branch_is_new {
        git::add_worktree(repo_path, &work_path, &branch, base_ref.as_deref())?;
    } else {
        git::add_worktree_on_branch(repo_path, &work_path, &branch)?;
    }

    Ok(Prepared {
        slug,
        branch: Some(branch),
        base_ref,
        work_path,
        is_git: true,
        uses_worktree: true,
        branch_is_new,
    })
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct Runner {
    id: String,
    db: Db,
    bus: broadcast::Sender<ServerMsg>,
    /// Every live agent's pid, so a sweep never adopts a sibling's group.
    agent_pids: Arc<RwLock<HashSet<i32>>>,
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
    /// Control requests we have sent and not yet seen answered. A response for
    /// anything else is not ours to act on.
    outstanding: HashSet<String>,
    /// The id of the `initialize` handshake sent at launch, until it is
    /// answered. Answering it is what moves the agent out of `Starting`.
    init_request_id: Option<String>,
    commands: Arc<RwLock<Vec<SlashCommand>>>,
    stop_requested: bool,
    last_stderr: Option<String>,
    /// Process groups started by this agent's tool calls, accumulated as the
    /// session runs. Shared with the refresher tasks. `claude` puts each Bash
    /// tool call in a new group, and by the time the CLI is gone its
    /// descendants have reparented to init — so the list has to be built while
    /// the agent is alive, not looked up when it dies.
    sweep: Arc<RwLock<Sweep>>,
    /// When the sweep was last refreshed, so activity can keep the ownership
    /// proof fresh without a `ps` per event.
    last_refresh: std::time::Instant,
    /// How long a process group gets between SIGTERM and SIGKILL. A field so
    /// tests can drive the escalation without waiting out the real grace.
    grace: Duration,
    /// Messages we wrote to stdin and already persisted. The CLI echoes user
    /// lines back on stdout; without this the transcript shows them twice.
    recently_sent: std::collections::VecDeque<String>,
    /// The supervisor's rate-limit snapshot, written through on every
    /// `rate_limit_event` this agent's CLI reports.
    rate_limit: Arc<RwLock<Option<RateLimitInfo>>>,
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
                        let known = self.sweep_snapshot().await;
                        let forbidden = self.forbidden_groups().await;
                        self.child.stop(STOP_GRACE, known, forbidden);
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

    /// Read the accumulated sweep.
    async fn sweep_snapshot(&self) -> Sweep {
        self.sweep.read().await.clone()
    }

    /// Process groups no sweep may ever adopt: every other live agent's.
    async fn forbidden_groups(&self) -> Vec<i32> {
        self.agent_pids.read().await.iter().copied().collect()
    }

    /// Run a snapshot cycle and fold the result into what we already know.
    ///
    /// Scheduled off the runner loop so a `ps` never delays event handling.
    /// Several snapshots follow each tool call starting. The later ones do the
    /// real work — they record a child that is still running, which is what
    /// makes it reachable at teardown. The earliest are a cheap best effort at
    /// a shell that exits immediately; they occasionally win, but they do not
    /// close the backgrounded-job case (DESIGN.md §4).
    fn refresh_sweep(&self, delay: Duration) {
        let pid = self.child.pid as i32;
        if pid <= 0 {
            return;
        }
        let shared = self.sweep.clone();
        let siblings = self.agent_pids.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            // Held across the cycle so two refreshes cannot interleave and undo
            // each other's pruning.
            let forbidden: Vec<i32> = siblings.read().await.iter().copied().collect();
            let mut guard = shared.write().await;
            let known = guard.clone();
            if let Ok(updated) =
                tokio::task::spawn_blocking(move || process::refresh_sweep(known, pid, &forbidden))
                    .await
            {
                *guard = updated;
            }
        });
    }

    /// SIGTERM, grace, SIGKILL for the process groups this agent's tool calls
    /// started. Returns the groups it acted on, for the exit record.
    ///
    /// This runs on **every** exit path — an operator Stop, a crash, a budget
    /// exhaustion, a non-zero exit — and it is awaited, so `run()` does not
    /// return (and the agent is not deregistered) until the escalation has had
    /// its chance. Shutdown waits on exactly that.
    async fn tear_down_groups(&mut self) -> Vec<i32> {
        let sweep = self.sweep_snapshot().await;
        if sweep.is_empty() {
            return Vec::new();
        }
        let probe = sweep.clone();
        let alive = tokio::task::spawn_blocking(move || process::surviving_groups(&probe))
            .await
            .unwrap_or_default();
        tracing::debug!(
            agent = %self.id,
            known = ?sweep.group_ids(),
            ?alive,
            "tearing down tool-call process groups"
        );
        if alive.is_empty() {
            return Vec::new();
        }

        tracing::info!(agent = %self.id, groups = ?alive, "terminating process groups left by tool calls");
        self.child
            .signal_groups(&alive, nix::sys::signal::Signal::SIGTERM);

        // Poll rather than sleeping out the whole grace: a group that dies on
        // SIGTERM — nearly all of them — should not hold the agent open, and
        // hold the operator's Resume with it, for five seconds.
        let poll = Duration::from_millis(250).min(self.grace);
        let deadline = tokio::time::Instant::now() + self.grace;
        loop {
            tokio::time::sleep(poll).await;
            let probe = sweep.clone();
            let left = tokio::task::spawn_blocking(move || process::surviving_groups(&probe))
                .await
                .unwrap_or_default();
            if left.is_empty() {
                return alive;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(agent = %self.id, groups = ?left, "process groups ignored SIGTERM; sending SIGKILL");
                self.child
                    .signal_groups(&left, nix::sys::signal::Signal::SIGKILL);
                return alive;
            }
        }
    }

    /// Keep the ownership proof fresh while the agent is doing anything.
    ///
    /// A group is only provably ours while something in it predates our last
    /// proof, so a long-lived process that forks workers and then loses its
    /// parent stays reachable only if the proof keeps moving. Throttled, and
    /// skipped entirely until there is something to keep proof of.
    fn refresh_on_activity(&mut self) {
        const EVERY: Duration = Duration::from_secs(2);
        if self.last_refresh.elapsed() < EVERY {
            return;
        }
        let tracking = self
            .sweep
            .try_read()
            .map(|sweep| !sweep.is_empty())
            .unwrap_or(false);
        if !tracking {
            return;
        }
        self.last_refresh = std::time::Instant::now();
        self.refresh_sweep(Duration::ZERO);
    }

    async fn on_action(&mut self, action: Action) {
        match action {
            Action::Persist { kind, payload } => {
                if kind == EventKind::User && self.is_echo(&payload) {
                    return;
                }
                self.refresh_on_activity();
                match kind {
                    // A tool call is starting. Sample a few times over the
                    // first quarter-second: what matters is catching a child
                    // that is still running later, and the early samples are a
                    // cheap best effort at a shell that exits at once. They are
                    // NOT a fix for a job backgrounded with `&` — measured
                    // against the real CLI those reparent to pid 1 before the
                    // first sample can run and are not swept (DESIGN.md §4).
                    EventKind::ToolUse => {
                        for ms in [0, 8, 20, 45, 90, 250] {
                            self.refresh_sweep(Duration::from_millis(ms));
                        }
                    }
                    // A tool call returned, or the turn ended. Anything still
                    // running under the CLI now — a dev server, a build — is
                    // exactly what has to be caught, and is what the teardown
                    // reliably reaches.
                    EventKind::ToolResult | EventKind::Result => self.refresh_sweep(Duration::ZERO),
                    _ => {}
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
                if self.pending.contains_key(&request.request_id) {
                    // Request ids are the child's to choose. Overwriting a live
                    // entry would let a second prompt inherit the operator's
                    // answer to the first.
                    tracing::warn!(
                        agent = %self.id,
                        request_id = %request.request_id,
                        "ignored a permission request reusing a pending id"
                    );
                    self.emit(ServerMsg::Notice {
                        agent_id: Some(self.id.clone()),
                        level: "warn".to_string(),
                        text: format!(
                            "The agent asked twice with the same request id ({}); the second was ignored.",
                            request.request_id
                        ),
                    });
                    return;
                }
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
                if !self.outstanding.remove(&request_id) {
                    tracing::warn!(
                        agent = %self.id,
                        %request_id,
                        "ignored a control response to a request we never sent"
                    );
                    return;
                }
                if is_error {
                    tracing::warn!(agent = %self.id, %request_id, ?payload, "control request failed");
                }
                if payload.get("still_queued").is_some() {
                    self.emit(ServerMsg::Queued {
                        agent_id: self.id.clone(),
                        still_queued: payload["still_queued"].clone(),
                    });
                }
                // The child answering our handshake is the only readiness
                // signal a launch is guaranteed to get: `system/init` is
                // emitted at the *start of a turn*, not at process start, so an
                // agent launched with no first message -- every resume -- would
                // otherwise sit at `Starting` until someone typed something.
                // An error reply still proves it is reading stdin and writing
                // stdout, which is all `Idle` claims.
                if self.init_request_id.as_deref() == Some(request_id.as_str()) {
                    self.init_request_id = None;
                    self.set_status(Transition::Initialized).await;
                }
            }
            Action::Commands {
                request_id,
                commands,
            } => {
                // Only a list answering a request of ours: an unsolicited one
                // would drive the operator's slash-command autocomplete.
                if !request_id.is_some_and(|id| self.outstanding.contains(&id)) {
                    tracing::warn!(agent = %self.id, "ignored an unsolicited command list");
                    return;
                }
                *self.commands.write().await = commands.clone();
                self.emit(ServerMsg::Commands {
                    agent_id: self.id.clone(),
                    commands,
                });
            }
            Action::SessionId(_) => {}
            Action::RateLimit(info) => {
                // Last writer wins: every agent's CLI reports the same account.
                *self.rate_limit.write().await = Some((*info).clone());
                self.emit(ServerMsg::RateLimit { info });
            }
            Action::Notice { level, text } => {
                tracing::info!(agent = %self.id, %level, %text, "agent notice");
                self.emit(ServerMsg::Notice {
                    agent_id: Some(self.id.clone()),
                    level,
                    text,
                });
            }
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
                let known = self.sweep_snapshot().await;
                let forbidden = self.forbidden_groups().await;
                self.child.stop(STOP_GRACE, known, forbidden);
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
        // Only a prompt that is actually outstanding may be answered: an
        // `allow` for anything else is a decision nobody asked for.
        let Some(request) = self.pending.remove(request_id) else {
            tracing::warn!(agent = %self.id, %request_id, "refused a decision for an unknown request");
            self.emit(ServerMsg::Notice {
                agent_id: Some(self.id.clone()),
                level: "warn".to_string(),
                text: "That approval is no longer outstanding — it was already answered, or \
                       belonged to a process that has since exited."
                    .to_string(),
            });
            return;
        };

        let requested_input = request.input.clone();
        let sent_input = match &decision {
            PermissionDecision::Allow { updated_input } => updated_input
                .clone()
                .unwrap_or_else(|| requested_input.clone()),
            PermissionDecision::Deny { .. } => Value::Null,
        };
        let modified =
            matches!(decision, PermissionDecision::Allow { .. }) && sent_input != requested_input;

        self.write(protocol::permission_response(
            request_id,
            &decision,
            &requested_input,
        ));
        // The log records what was actually approved, not merely that something
        // was: `updated_input` replaces the tool input outright, and a decision
        // that only said "allow" would misstate what ran.
        self.persist(
            EventKind::PermissionDecision,
            json!({
                "request_id": request_id,
                "behavior": decision.behavior(),
                "tool_name": request.tool_name,
                "input": sent_input,
                "input_modified": modified,
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
        self.outstanding.insert(id.clone());
        // A misbehaving child could otherwise make this grow without bound.
        if self.outstanding.len() > 256 {
            self.outstanding.clear();
            self.outstanding.insert(id.clone());
        }
        id
    }

    fn send_control(&mut self, build: fn(&str) -> Value) -> String {
        let id = self.take_request_id();
        let value = build(&id);
        self.write(value);
        id
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

        // Whatever the agent's tool calls left running is torn down here, not
        // in the Stop path: a crash, a budget exhaustion or a non-zero exit
        // leaks the same process groups as an ignored Stop would.
        let swept = self.tear_down_groups().await;

        self.persist(
            EventKind::System,
            json!({
                "type": "system",
                "subtype": "process_exit",
                "code": info.code,
                "signal": info.signal,
                "requested": requested,
                "swept_groups": swept,
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
            branch_is_new: true,
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
            Self::start_with(0, Sweep::default())
        }

        fn start_with(pid: u32, sweep: Sweep) -> Self {
            Self::start_with_grace(pid, sweep, Duration::from_millis(20))
        }

        fn start_with_grace(pid: u32, sweep: Sweep, grace: Duration) -> Self {
            let db = Db::open_in_memory().expect("db");
            let dir = std::env::temp_dir();
            let record = agent_record("agent-1", &dir);
            db.insert_agent(&record).expect("insert");

            let (bus, events) = broadcast::channel(256);
            let (child, stdin, stops) = ChildHandle::detached_with_pid(pid);
            let (msg_tx, msg_rx) = mpsc::unbounded_channel();
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

            let runner = Runner {
                id: record.id.clone(),
                db: db.clone(),
                bus,
                agent_pids: Arc::new(RwLock::new(HashSet::new())),
                child,
                msgs: msg_rx,
                cmd_rx,
                cmd_closed: false,
                status: Status::Starting,
                status_detail: None,
                cost_usd: 0.0,
                pending: HashMap::new(),
                next_request_id: 1,
                outstanding: HashSet::new(),
                init_request_id: None,
                commands: Arc::new(RwLock::new(Vec::new())),
                stop_requested: false,
                last_stderr: None,
                sweep: Arc::new(RwLock::new(sweep)),
                last_refresh: std::time::Instant::now(),
                grace,
                recently_sent: std::collections::VecDeque::new(),
                rate_limit: Arc::new(RwLock::new(None)),
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

    /// A CLI that answers the `initialize` handshake and nothing else — which
    /// is exactly what a resumed session does until it is sent a message.
    fn handshake_only_cli(dir: &Path) -> Option<String> {
        let path = dir.join("handshake-cli");
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             while IFS= read -r line; do\n\
             id=$(printf '%s' \"$line\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\n\
             [ -n \"$id\" ] && printf '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"%s\",\"response\":{}}}\\n' \"$id\"\n\
             done\n",
        )
        .ok()?;
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

    /// A resume sends no first message, and the CLI emits `system/init` only at
    /// the start of a turn — so nothing but the handshake reply can move the
    /// agent out of `Starting`. Without that, Resume left the agent showing
    /// "starting" for as long as it ran.
    #[tokio::test]
    async fn a_resume_reaches_idle_without_a_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(bin) = handshake_only_cli(dir.path()) else {
            return;
        };
        let db = Db::open_in_memory().expect("db");
        let record = agent_record("agent-resume-idle", dir.path());
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(
            db.clone(),
            Arc::new(RwLock::new(Config {
                claude_bin: bin,
                ..Config::default()
            })),
        );
        let mut rx = sup.subscribe();
        sup.resume(&record.id).await.expect("launch");

        let idle = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(ServerMsg::Status { status, .. }) = rx.recv().await
                    && status == Status::Idle
                {
                    return;
                }
            }
        })
        .await;
        assert!(
            idle.is_ok(),
            "the handshake reply must take the agent out of `starting`"
        );
        assert_eq!(
            db.get_agent(&record.id)
                .expect("get")
                .expect("present")
                .status,
            Status::Idle,
            "and the status has to be persisted, not only broadcast"
        );

        sup.shutdown().await;
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

    // -- teardown on every exit path, and shutdown waiting for it ------------

    /// A real, throwaway process in a process group of its own — the shape a
    /// Bash tool call leaves behind. No `claude`, no network, no ports.
    struct GroupLeader {
        child: std::process::Child,
    }

    impl GroupLeader {
        fn start() -> Option<Self> {
            use std::os::unix::process::CommandExt;
            if !Path::new("/bin/sh").exists() {
                return None;
            }
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "sleep 30"]);
            command.process_group(0);
            command.spawn().ok().map(|child| Self { child })
        }

        fn pid(&self) -> i32 {
            self.child.id() as i32
        }

        /// `process_group(0)` makes the child its own group leader, so the
        /// group id is the pid. Recorded as seen just now, exactly as a live
        /// snapshot would have.
        fn sweep(&self) -> Sweep {
            Sweep {
                groups: vec![crate::agent::process::GroupRecord {
                    pgid: self.pid(),
                    first_seen_ms: crate::db::now_ms(),
                    witness_pid: self.pid(),
                    witness_started_ms: crate::db::now_ms(),
                    proven_ms: crate::db::now_ms(),
                }],
            }
        }

        fn is_alive(&self) -> bool {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.pid()), None).is_ok()
        }

        /// Wait for the process to be reaped, bounded so a failure is a failure
        /// rather than a hang.
        async fn wait_until_gone(&mut self) -> bool {
            for _ in 0..400 {
                // A zombie is still "alive" to kill(2) until it is waited on.
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return true;
                }
                if !self.is_alive() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        }
    }

    impl Drop for GroupLeader {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[tokio::test]
    async fn a_cli_that_exits_on_its_own_still_gets_its_tool_groups_swept() {
        let Some(mut leader) = GroupLeader::start() else {
            return;
        };
        assert!(
            leader.is_alive(),
            "the stand-in tool process should be running"
        );

        // The runner knows about the group (it was snapshotted while the agent
        // ran) and the CLI now dies on its own — a crash, not a Stop.
        let harness = Harness::start_with(std::process::id(), leader.sweep());
        let id = harness.id.clone();
        harness
            .msgs
            .send(ProcessMsg::Exited(ExitInfo {
                code: Some(9),
                signal: None,
                requested: false,
            }))
            .expect("runner is alive");
        harness.task.await.expect("runner should finish");

        assert!(
            leader.wait_until_gone().await,
            "a process group left by a tool call must be torn down even when the CLI was never Stopped"
        );

        // The teardown is recorded in the agent's own log, not just in tracing.
        let exit = harness
            .db
            .events_after(&id, 0, 100)
            .expect("events")
            .into_iter()
            .find(|e| e.payload["subtype"] == json!("process_exit"))
            .expect("an exit event");
        assert_eq!(
            exit.payload["requested"],
            json!(false),
            "not an operator Stop"
        );
        assert_eq!(
            exit.payload["swept_groups"],
            json!([leader.pid()]),
            "the swept groups belong in the record"
        );
    }

    #[tokio::test]
    async fn the_teardown_finishes_before_the_runner_reports_itself_done() {
        let Some(mut leader) = GroupLeader::start() else {
            return;
        };
        let harness = Harness::start_with(std::process::id(), leader.sweep());
        harness
            .msgs
            .send(ProcessMsg::Exited(ExitInfo {
                code: Some(0),
                signal: None,
                requested: true,
            }))
            .expect("runner is alive");

        // The ordering shutdown depends on: run() returning means the teardown
        // is done, which means deregistration means running_count() == 0.
        harness.task.await.expect("runner should finish");
        assert!(
            matches!(leader.child.try_wait(), Ok(Some(_))) || !leader.is_alive(),
            "the group must already be gone by the time the runner reports done"
        );
    }

    #[tokio::test]
    async fn shutdown_does_not_return_until_every_runner_has_finished() {
        let db = Db::open_in_memory().expect("db");
        let sup = Supervisor::new(db, Arc::new(RwLock::new(Config::default())));
        let (tx, mut rx) = mpsc::unbounded_channel();
        sup.runners.write().await.insert(
            "a".to_string(),
            RunnerHandle {
                tx,
                commands: Arc::new(RwLock::new(Vec::new())),
                generation: 7,
            },
        );

        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runners = sup.runners.clone();
        let flag = finished.clone();
        tokio::spawn(async move {
            // The Stop that shutdown() sends.
            assert!(matches!(rx.recv().await, Some(AgentCommand::Stop)));
            // Stand-in for the teardown: signal, grace, escalate.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            flag.store(true, std::sync::atomic::Ordering::Release);
            deregister(&runners, "a", 7).await;
        });

        sup.shutdown().await;
        assert!(
            finished.load(std::sync::atomic::Ordering::Acquire),
            "shutdown must not return while a runner is still tearing down"
        );
        assert_eq!(sup.running_count().await, 0);
    }

    // -- what a decision records --------------------------------------------

    #[tokio::test]
    async fn an_edited_approval_records_what_was_actually_sent() {
        let mut harness = Harness::start();
        harness.action(Action::Permission(Box::new(PermissionRequest {
            request_id: "abc".to_string(),
            tool_name: "Bash".to_string(),
            display_name: None,
            description: None,
            tool_use_id: None,
            input: json!({"command": "cargo test"}),
            permission_suggestions: Value::Null,
        })));
        harness.action(Action::Transition(Transition::PermissionRequested));
        harness.next_status().await;

        // The operator approves, but with a different command than the one the
        // agent asked for. `updated_input` replaces the tool input outright.
        harness
            .cmds
            .as_ref()
            .expect("sender")
            .send(AgentCommand::Decide {
                request_id: "abc".to_string(),
                decision: PermissionDecision::Allow {
                    updated_input: Some(json!({"command": "cargo test --lib"})),
                },
            })
            .expect("runner is alive");
        harness.next_status().await;

        let id = harness.id.clone();
        let db = harness.finish().await;
        let decision = db
            .events_after(&id, 0, 100)
            .expect("events")
            .into_iter()
            .find(|e| e.kind == "permission_decision")
            .expect("a decision event");
        assert_eq!(decision.payload["behavior"], json!("allow"));
        assert_eq!(
            decision.payload["input"],
            json!({"command": "cargo test --lib"}),
            "the log must say what actually ran, not merely that something was approved"
        );
        assert_eq!(decision.payload["input_modified"], json!(true));
        assert_eq!(decision.payload["tool_name"], json!("Bash"));
    }

    #[tokio::test]
    async fn a_decision_for_a_request_that_is_not_pending_is_refused() {
        let mut harness = Harness::start();
        harness
            .cmds
            .as_ref()
            .expect("sender")
            .send(AgentCommand::Decide {
                request_id: "never-asked".to_string(),
                decision: PermissionDecision::Allow {
                    updated_input: None,
                },
            })
            .expect("runner is alive");

        // The runner answers with a notice rather than an approval.
        let notice = loop {
            match harness.events.recv().await.expect("bus") {
                ServerMsg::Notice { text, .. } => break text,
                _ => continue,
            }
        };
        assert!(notice.contains("no longer outstanding"), "{notice}");

        let id = harness.id.clone();
        let db = harness.finish().await;
        assert!(
            !db.events_after(&id, 0, 100)
                .expect("events")
                .iter()
                .any(|e| e.kind == "permission_decision"),
            "nothing may be recorded as approved"
        );
    }

    // -- the SIGTERM -> SIGKILL escalation -----------------------------------

    /// A group leader that ignores SIGTERM, so only the escalation ends it.
    fn stubborn_leader() -> Option<GroupLeader> {
        use std::os::unix::process::CommandExt;
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "trap '' TERM; sleep 30"]);
        command.process_group(0);
        command.spawn().ok().map(|child| GroupLeader { child })
    }

    #[tokio::test]
    async fn a_group_that_ignores_sigterm_is_killed() {
        let Some(mut leader) = stubborn_leader() else {
            return;
        };
        // Give SIGTERM a moment to be installed as ignored before we send it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(leader.is_alive());

        let harness = Harness::start_with(std::process::id(), leader.sweep());
        harness
            .msgs
            .send(ProcessMsg::Exited(ExitInfo {
                code: Some(0),
                signal: None,
                requested: true,
            }))
            .expect("runner is alive");
        harness.task.await.expect("runner should finish");

        assert!(
            leader.wait_until_gone().await,
            "a group that ignores SIGTERM must still be SIGKILLed"
        );
    }

    #[tokio::test]
    async fn a_group_that_obeys_sigterm_is_not_waited_out() {
        let Some(mut leader) = GroupLeader::start() else {
            return;
        };
        // A grace long enough that sitting it out would be obvious.
        let harness =
            Harness::start_with_grace(std::process::id(), leader.sweep(), Duration::from_secs(5));
        harness
            .msgs
            .send(ProcessMsg::Exited(ExitInfo {
                code: Some(0),
                signal: None,
                requested: true,
            }))
            .expect("runner is alive");

        let started = std::time::Instant::now();
        harness.task.await.expect("runner should finish");
        assert!(
            leader.wait_until_gone().await,
            "SIGTERM should have ended it"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a group that dies on SIGTERM must not hold the agent open for the whole grace: {:?}",
            started.elapsed()
        );
    }

    // -- how long a delete is prepared to wait -------------------------------

    #[tokio::test(start_paused = true)]
    async fn delete_waits_for_the_whole_teardown_not_just_the_child_grace() {
        let db = Db::open_in_memory().expect("db");
        let dir = tempfile::tempdir().expect("tempdir");
        let record = agent_record("agent-slow", dir.path());
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(db.clone(), Arc::new(RwLock::new(Config::default())));

        let (tx, mut rx) = mpsc::unbounded_channel();
        sup.runners.write().await.insert(
            record.id.clone(),
            RunnerHandle {
                tx,
                commands: Arc::new(RwLock::new(Vec::new())),
                generation: 1,
            },
        );

        // A stop that needs the child's own grace and then a group teardown:
        // longer than one grace, shorter than the deadline delete must honour.
        let runners = sup.runners.clone();
        let id = record.id.clone();
        tokio::spawn(async move {
            assert!(matches!(rx.recv().await, Some(AgentCommand::Stop)));
            tokio::time::sleep(STOP_GRACE + Duration::from_secs(3)).await;
            deregister(&runners, &id, 1).await;
        });

        sup.delete(&record.id, false, false).await.expect("delete");
        assert_eq!(
            sup.running_count().await,
            0,
            "delete must not tear down a worktree while the agent is still stopping"
        );
        assert!(
            Supervisor::teardown_deadline() >= STOP_GRACE * 2,
            "the deadline has to cover the child's grace and the group teardown"
        );
    }

    // -- workspace preparation ------------------------------------------------

    /// A real repository on `main`, with one commit and one extra branch.
    fn repo_with_a_spare_branch() -> Option<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir().ok()?;
        let path = dir.path().join("repo");
        std::fs::create_dir_all(&path).ok()?;
        git::git(&path, &["init", "-q", "-b", "main", "."]).ok()?;
        git::git(&path, &["config", "user.email", "t@example.com"]).ok()?;
        git::git(&path, &["config", "user.name", "Test"]).ok()?;
        std::fs::write(path.join("README.md"), "hello").ok()?;
        git::git(&path, &["add", "."]).ok()?;
        git::git(&path, &["commit", "-q", "-m", "initial"]).ok()?;
        git::git(&path, &["branch", "feature_login"]).ok()?;
        Some((dir, path))
    }

    fn spawn_req(repo: &Path) -> SpawnRequest {
        SpawnRequest {
            repo_path: repo.to_string_lossy().to_string(),
            task_name: "Fix the parser".to_string(),
            base_ref: None,
            model: None,
            effort: None,
            max_budget_usd: None,
            permission_mode: None,
            in_place: false,
            existing_branch: None,
            add_dirs: Vec::new(),
            first_message: None,
        }
    }

    #[test]
    fn a_new_branch_is_named_from_the_task_and_owned_by_the_agent() {
        let Some((_dir, repo)) = repo_with_a_spare_branch() else {
            return;
        };
        let prepared = prepare_workspace(
            &repo,
            "Fix the parser",
            "sw_",
            &HashSet::new(),
            &spawn_req(&repo),
        )
        .expect("prepare");
        assert_eq!(prepared.branch.as_deref(), Some("sw_fix_the_parser"));
        assert_eq!(prepared.base_ref.as_deref(), Some("main"));
        assert!(prepared.uses_worktree);
        assert!(
            prepared.branch_is_new,
            "a branch we created is ours to delete"
        );
    }

    #[test]
    fn an_existing_branch_is_joined_rather_than_recreated() {
        let Some((_dir, repo)) = repo_with_a_spare_branch() else {
            return;
        };
        let mut req = spawn_req(&repo);
        req.existing_branch = Some("feature_login".to_string());
        let prepared = prepare_workspace(&repo, "Fix the parser", "sw_", &HashSet::new(), &req)
            .expect("prepare");

        assert_eq!(prepared.branch.as_deref(), Some("feature_login"));
        assert!(
            !prepared.branch_is_new,
            "a branch that predates the agent is not ours to delete"
        );
        // No base ref: the branch has its own head, and a start point would
        // move it. Stored as None so the delete check stays conservative.
        assert_eq!(prepared.base_ref, None);
        // The slug still comes from the task name, so the agent and its
        // worktree directory are named independently of the branch.
        assert_eq!(prepared.slug, "fix_the_parser");
        assert_eq!(
            git::current_branch(&prepared.work_path).as_deref(),
            Some("feature_login")
        );
    }

    /// The reuse path must never create a branch, so a name that is not already
    /// one is refused before any git command runs.
    #[test]
    fn reusing_a_branch_that_does_not_exist_is_refused() {
        let Some((_dir, repo)) = repo_with_a_spare_branch() else {
            return;
        };
        let mut req = spawn_req(&repo);
        req.existing_branch = Some("invented".to_string());
        let err = prepare_workspace(&repo, "Fix the parser", "sw_", &HashSet::new(), &req)
            .expect_err("must refuse");
        assert!(format!("{err:#}").contains("not a branch"), "{err:#}");
        assert_eq!(git::list_branches(&repo).len(), 2, "nothing may be created");
    }

    /// A base ref is meaningless alongside a reused branch. It must be dropped,
    /// not applied — applying it would move someone else's branch.
    #[test]
    fn a_base_ref_is_ignored_when_a_branch_is_reused() {
        let Some((_dir, repo)) = repo_with_a_spare_branch() else {
            return;
        };
        let head = git::resolve_commit(&repo, "feature_login").expect("head");
        let mut req = spawn_req(&repo);
        req.existing_branch = Some("feature_login".to_string());
        req.base_ref = Some("main".to_string());
        let prepared = prepare_workspace(&repo, "Fix the parser", "sw_", &HashSet::new(), &req)
            .expect("prepare");
        assert_eq!(prepared.base_ref, None);
        assert_eq!(
            git::resolve_commit(&repo, "feature_login").expect("head after"),
            head,
            "the reused branch must not have moved"
        );
    }

    #[test]
    fn reusing_a_branch_in_place_moves_the_main_checkout_onto_it() {
        let Some((_dir, repo)) = repo_with_a_spare_branch() else {
            return;
        };
        let mut req = spawn_req(&repo);
        req.existing_branch = Some("feature_login".to_string());
        req.in_place = true;
        let prepared = prepare_workspace(&repo, "Fix the parser", "sw_", &HashSet::new(), &req)
            .expect("prepare");
        assert!(!prepared.uses_worktree);
        assert_eq!(prepared.work_path, repo);
        assert_eq!(git::current_branch(&repo).as_deref(), Some("feature_login"));
    }

    // -- the permission control plane ----------------------------------------

    #[tokio::test]
    async fn relaxing_the_permission_mode_needs_an_explicit_confirmation() {
        let db = Db::open_in_memory().expect("db");
        let dir = tempfile::tempdir().expect("tempdir");
        let record = agent_record("agent-perm", dir.path());
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(db.clone(), Arc::new(RwLock::new(Config::default())));

        let err = sup
            .set_permission_mode(&record.id, PermissionMode::Bypass, false)
            .await
            .expect_err("an unconfirmed relaxation must be refused");
        assert!(format!("{err:#}").contains("more freedom"), "{err:#}");
        assert_eq!(
            db.get_agent(&record.id)
                .expect("get")
                .expect("present")
                .permission_mode,
            PermissionMode::Ask,
            "nothing may change on a refusal"
        );

        // Tightening never needs confirmation.
        sup.set_permission_mode(&record.id, PermissionMode::Ask, false)
            .await
            .expect("no change is fine");

        // Confirmed, it goes through — and lands in the agent's own log.
        sup.set_permission_mode(&record.id, PermissionMode::Bypass, true)
            .await
            .expect("confirmed");
        let agent = db.get_agent(&record.id).expect("get").expect("present");
        assert_eq!(agent.permission_mode, PermissionMode::Bypass);
        let change = db
            .events_after(&record.id, 0, 100)
            .expect("events")
            .into_iter()
            .find(|e| e.payload["subtype"] == json!("permission_mode_change"))
            .expect("the change must be recorded in the transcript");
        assert_eq!(change.payload["from"], json!("ask"));
        assert_eq!(change.payload["to"], json!("bypass"));
        assert_eq!(change.payload["relaxed"], json!(true));
        assert_eq!(change.payload["initiator"], json!("operator"));
    }

    #[tokio::test]
    async fn dangerous_cannot_be_applied_to_a_running_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(bin) = stub_cli(dir.path()) else {
            return;
        };
        let db = Db::open_in_memory().expect("db");
        let record = agent_record("agent-danger", dir.path());
        db.insert_agent(&record).expect("insert");
        let sup = Supervisor::new(
            db.clone(),
            Arc::new(RwLock::new(Config {
                claude_bin: bin,
                ..Config::default()
            })),
        );
        sup.resume(&record.id).await.expect("launch");

        let err = sup
            .set_permission_mode(&record.id, PermissionMode::Dangerous, true)
            .await
            .expect_err("there is no runtime equivalent of the launch flag");
        assert!(
            format!("{err:#}").contains("only be applied at launch"),
            "{err:#}"
        );
        assert_eq!(
            db.get_agent(&record.id)
                .expect("get")
                .expect("present")
                .permission_mode,
            PermissionMode::Ask,
            "the recorded mode must not diverge from the one in force"
        );

        sup.shutdown().await;
    }
}
