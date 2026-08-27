//! Child process lifecycle: spawn, stdin writer, stdout/stderr readers, signals.
//!
//! The interpretation of the CLI's output lives in [`Dispatcher`], which is a
//! pure function from a line of text to a list of [`Action`]s. The transport
//! below only moves bytes, so the protocol handling can be tested against
//! synthetic stdout without a real `claude` binary anywhere near it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::protocol::{
    self, CliEvent, EventKind, LaunchArgs, PermissionRequest, RateLimitInfo, SlashCommand,
    ToolProgressLine, tool_uses,
};
use super::state::Transition;

/// How long stdout is drained after the child exits, before the exit is
/// reported.
///
/// Bounded because *any* child that inherited the CLI's stdout pipe keeps it
/// open, and waiting for EOF would then hang forever — the agent would sit in
/// `Idle`, never failing and never resumable.
///
/// Measured against `claude` 2.1.241: its **Bash tool calls** do not inherit
/// this pipe (their fds are `/dev/null` and a temporary file), so that
/// particular case cannot arise. The bound stays because it costs 500ms on an
/// exit that has already happened, it holds for anything else the CLI spawns
/// that does inherit stdout, and the alternative failure is unbounded. The test
/// `an_exit_is_reported_even_when_a_grandchild_holds_the_stdout_pipe`
/// demonstrates the failure mode with a stub that does inherit it.
const DRAIN_AFTER_EXIT: std::time::Duration = std::time::Duration::from_millis(500);

/// One consequence of a line of CLI output.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this to the events table under `kind`.
    Persist { kind: EventKind, payload: Value },
    /// A partial-token delta: broadcast live, never persisted (§3).
    Partial(Value),
    /// Drive the status state machine.
    Transition(Transition),
    /// Replace the `Working` sub-label.
    StatusDetail(Option<String>),
    /// The turn's cumulative cost.
    Cost(f64),
    /// A tool permission prompt needing a human.
    Permission(Box<PermissionRequest>),
    /// The CLI answered one of our control requests.
    ControlResponse {
        request_id: String,
        payload: Value,
        is_error: bool,
    },
    /// The slash command list from `initialize` (F9), tagged with the request
    /// it answers so an unsolicited list cannot be injected.
    Commands {
        request_id: Option<String>,
        commands: Vec<SlashCommand>,
    },
    /// The session id the CLI reported.
    SessionId(String),
    /// Account usage against the rate-limit windows. Account-wide, not this
    /// agent's: the last one to arrive is the truth for every agent.
    RateLimit(Box<RateLimitInfo>),
    /// Something the operator should see as a toast, with no transcript entry
    /// of its own — a subagent retrying, say.
    Notice { level: String, text: String },
    /// A line we could not classify — logged and surfaced, never fatal.
    Unrecognised { kind: String, reason: String },
}

/// The tool a heartbeat is about, given the heartbeat's own `tool_use_id`.
///
/// The CLI mints that id as `<real tool_use_id>-heartbeat-<n>`, so it never
/// matches a `tool_use` block on its own. `None` when the id is not in that
/// shape, which is the signal that this reasoning does not apply.
fn heartbeat_origin(tool_use_id: &str) -> Option<&str> {
    let (origin, counter) = tool_use_id.rsplit_once("-heartbeat-")?;
    (!origin.is_empty() && !counter.is_empty() && counter.bytes().all(|b| b.is_ascii_digit()))
        .then_some(origin)
}

/// `90` → `"1m30s"`. Heartbeats arrive every 30s, so minutes matter quickly.
fn fmt_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let (m, s) = (seconds / 60, seconds % 60);
    if m < 60 {
        return format!("{m}m{s:02}s");
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

/// Turns CLI output lines into [`Action`]s. Holds only the small amount of
/// state needed to notice a missing `init`.
#[derive(Debug, Default)]
pub struct Dispatcher {
    saw_init: bool,
    /// `tool_use_id` → the label shown while that tool runs, so a heartbeat can
    /// say "Bash: cargo test — 60s" instead of the bare tool name. Tool ids do
    /// not outlive their turn, so this is cleared at every `result`.
    tool_labels: HashMap<String, String>,
}

/// How many in-flight tool labels to remember before dropping the lot. A bound
/// rather than a cache: a turn wide enough to hit it loses labels, not
/// heartbeats — an unmatched heartbeat still reports progress, just under the
/// bare tool name.
const MAX_TRACKED_TOOLS: usize = 64;

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once a `system`/`init` line has been seen, which is the startup
    /// assertion from the risk register.
    pub fn saw_init(&self) -> bool {
        self.saw_init
    }

    /// The `Working` label for a heartbeat: the running tool's own label when we
    /// still have it, the bare tool name otherwise, with the elapsed time
    /// appended.
    ///
    /// `None` when the line carries no elapsed time, which would make the label
    /// a step backwards from whatever is already showing.
    fn progress_label(&self, prog: &ToolProgressLine) -> Option<String> {
        let elapsed = prog.elapsed_time_seconds?;
        let id = prog.tool_use_id.as_deref();
        // A heartbeat's own id is synthetic; the tool it is about is
        // `heartbeat_origin`, falling back to the parent for anything else.
        let origin = id
            .and_then(heartbeat_origin)
            .or(prog.parent_tool_use_id.as_deref());
        let base = origin
            .or(id)
            .and_then(|id| self.tool_labels.get(id))
            .cloned()
            .or_else(|| prog.tool_name.clone())?;
        Some(format!("{base} — {}", fmt_elapsed(elapsed)))
    }

    /// Interpret one line of stdout.
    pub fn on_stdout(&mut self, line: &str) -> Vec<Action> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let parsed = protocol::parse_line(line);
        tracing::trace!(kind = parsed.event.type_name(), "cli line");
        let raw = parsed.raw;
        let mut out = Vec::new();

        match parsed.event {
            CliEvent::System(sys) => {
                out.push(Action::Persist {
                    kind: EventKind::System,
                    payload: raw,
                });
                if let Some(id) = sys.session_id {
                    out.push(Action::SessionId(id));
                }
                if sys.subtype.as_deref() == Some("init") {
                    self.saw_init = true;
                    out.push(Action::Transition(Transition::Initialized));
                }
            }
            CliEvent::Assistant(msg) if msg.is_api_error_message => {
                // A synthesised line carrying an API failure, not the model
                // talking. Filed as an error so the transcript does not put
                // "You've hit your session limit" in Claude's mouth. No toast:
                // the `result` that closes the turn raises one, and a rate
                // limit produces several of these per turn.
                tracing::warn!(
                    agent_error = msg.error.as_deref().unwrap_or("unknown"),
                    "the CLI reported an API error"
                );
                out.push(Action::Persist {
                    kind: EventKind::Error,
                    payload: raw,
                });
            }
            CliEvent::Assistant(msg) => {
                out.push(Action::Persist {
                    kind: EventKind::Assistant,
                    payload: raw,
                });
                for use_ in tool_uses(&msg) {
                    let label = use_.label();
                    if let Some(id) = use_.id.clone() {
                        if self.tool_labels.len() >= MAX_TRACKED_TOOLS {
                            self.tool_labels.clear();
                        }
                        self.tool_labels.insert(id, label.clone());
                    }
                    out.push(Action::StatusDetail(Some(label)));
                    out.push(Action::Persist {
                        kind: EventKind::ToolUse,
                        payload: json!({
                            "id": use_.id,
                            "name": use_.name,
                            "input": use_.input,
                        }),
                    });
                }
            }
            CliEvent::User(msg) => {
                let kind = if protocol::has_tool_result(&msg) {
                    EventKind::ToolResult
                } else {
                    EventKind::User
                };
                out.push(Action::Persist { kind, payload: raw });
            }
            CliEvent::Result(res) => {
                out.push(Action::Persist {
                    kind: EventKind::Result,
                    payload: raw,
                });
                if let Some(cost) = res.total_cost_usd {
                    out.push(Action::Cost(cost));
                }
                // A turn killed by a rate limit ends exactly like one that
                // finished the job: `TurnEnded`, then `Idle`. Say so, or the
                // operator is left looking at an idle agent that quietly
                // stopped halfway and no sign of why.
                if let Some(text) = res.failure() {
                    out.push(Action::Notice {
                        level: "error".to_string(),
                        text,
                    });
                }
                self.tool_labels.clear();
                out.push(Action::StatusDetail(None));
                out.push(Action::Transition(Transition::TurnEnded));
            }
            CliEvent::StreamEvent(value) => out.push(Action::Partial(value)),
            CliEvent::ControlRequest(req) => match req.as_permission_request() {
                Some(perm) => {
                    let payload = serde_json::to_value(&perm).unwrap_or_else(|_| raw.clone());
                    out.push(Action::Persist {
                        kind: EventKind::PermissionRequest,
                        payload,
                    });
                    out.push(Action::Transition(Transition::PermissionRequested));
                    out.push(Action::Permission(Box::new(perm)));
                }
                None => out.push(Action::Persist {
                    kind: EventKind::System,
                    payload: raw,
                }),
            },
            CliEvent::ControlResponse(res) => {
                out.push(Action::Persist {
                    kind: EventKind::System,
                    payload: raw,
                });
                if let Some(payload) = res.payload() {
                    let commands = protocol::commands_from_initialize(payload);
                    if !commands.is_empty() {
                        out.push(Action::Commands {
                            request_id: res.request_id().map(str::to_string),
                            commands,
                        });
                    }
                }
                if let Some(id) = res.request_id() {
                    out.push(Action::ControlResponse {
                        request_id: id.to_string(),
                        payload: res.payload().cloned().unwrap_or(Value::Null),
                        is_error: res.is_error(),
                    });
                }
            }
            CliEvent::RateLimit(line) => {
                // Not persisted: it is a gauge, not a transcript entry, and one
                // arrives per API request.
                out.push(Action::RateLimit(Box::new(line.rate_limit_info)));
            }
            CliEvent::ToolProgress(prog) => {
                // A retry is news; a heartbeat is only a label refresh. Neither
                // belongs in the transcript.
                if let Some(retry) = &prog.subagent_retry {
                    out.push(Action::Notice {
                        level: "warn".to_string(),
                        text: retry.describe(prog.subagent_type.as_deref()),
                    });
                } else if let Some(detail) = self.progress_label(&prog) {
                    out.push(Action::StatusDetail(Some(detail)));
                }
            }
            CliEvent::Unknown { kind, reason } => {
                // Nothing is lost to a parser gap: the raw line is still stored.
                out.push(Action::Persist {
                    kind: if kind == "<invalid json>" {
                        EventKind::Error
                    } else {
                        EventKind::System
                    },
                    payload: protocol::with_fields(
                        raw,
                        &[
                            ("_unrecognised", json!(true)),
                            ("_reason", json!(reason.clone())),
                        ],
                    ),
                });
                out.push(Action::Unrecognised { kind, reason });
            }
        }
        out
    }

    /// Interpret one line of stderr.
    pub fn on_stderr(&mut self, line: &str) -> Vec<Action> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        vec![Action::Persist {
            kind: EventKind::Stderr,
            payload: json!({"type": "stderr", "text": line}),
        }]
    }
}

/// A message from a running child.
#[derive(Debug)]
pub enum ProcessMsg {
    Action(Action),
    Exited(ExitInfo),
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// True when we asked it to stop, so an exit is not a failure.
    pub requested: bool,
}

/// Everything needed to launch a child.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub claude_bin: String,
    pub cwd: PathBuf,
    pub args: LaunchArgs,
}

/// A live child process.
pub struct ChildHandle {
    pub pid: u32,
    stdin: mpsc::UnboundedSender<Value>,
    exited: Arc<AtomicBool>,
    /// How many times a stop has been asked for. One per stop in normal
    /// operation; a runaway supervisor loop shows up here immediately.
    stop_requests: Arc<AtomicUsize>,
}

impl ChildHandle {
    /// Queue a line for the child's stdin.
    pub fn send(&self, value: Value) -> Result<()> {
        self.stdin
            .send(value)
            .map_err(|_| anyhow!("child stdin is closed"))
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// SIGTERM, so `SessionEnd` hooks run and the interrupted turn is recorded,
    /// then SIGKILL after the grace period (§4).
    ///
    /// Three things are signalled, not one:
    ///
    /// * the CLI's own process group (it is a group leader, see
    ///   `process_group(0)` in [`spawn`]);
    /// * every *other* process group found under the CLI in the process tree —
    ///   `claude` puts each Bash tool call in a **new** group of its own, so
    ///   `killpg` on the CLI's group structurally cannot reach a running
    ///   `cargo build` or `npm run dev` started by a tool call, and that is
    ///   exactly what holds the worktree open and breaks delete;
    /// * both again with SIGKILL once the grace period is up.
    ///
    /// The tree is snapshotted **before** the first signal, because SIGTERM to
    /// the CLI tears the tree down and there would be nothing left to walk.
    ///
    /// **Not** handled: anything that has already left the CLI's subtree by the
    /// time we look. That includes a job backgrounded with a plain `&` inside a
    /// tool call — measured against `claude` 2.1.241 on 2026-08-24, those are
    /// *not* swept (0 of 4 controlled trials), because the tool's shell exits
    /// within milliseconds and the job reparents to pid 1 before our first
    /// sample can run — as well as `nohup` and `setsid`. It is then in neither
    /// the CLI's group nor its subtree, and macOS has no cgroup equivalent to
    /// recover the relationship. What *is* reliably swept is a process still
    /// running as a descendant when the agent stops, crashes or the server
    /// shuts down: the build or dev server holding the worktree open. See
    /// DESIGN.md §4.
    pub fn stop(&self, grace: std::time::Duration, known: Sweep, forbidden: Vec<i32>) {
        self.stop_requests.fetch_add(1, Ordering::AcqRel);
        if self.has_exited() {
            return;
        }
        let pid = self.pid;
        let exited = self.exited.clone();
        tokio::spawn(async move {
            // The tree is snapshotted before anything is signalled: SIGTERM to
            // the CLI tears it down and there would be nothing left to walk.
            // Reading the process table shells out, so keep it off the runtime.
            //
            // The targets go through the same ownership test as every other
            // signal: the accumulated half of `known` holds ids recorded a long
            // time ago, and a group id whose processes have gone may since have
            // been recycled onto something of the user's.
            let groups = tokio::task::spawn_blocking(move || {
                stop_targets_now(&known, pid as i32, &forbidden)
            })
            .await
            .unwrap_or_default();
            if !groups.is_empty() {
                tracing::info!(
                    pid,
                    ?groups,
                    "stopping process groups started by the agent's tool calls"
                );
            }

            signal_group(pid, nix::sys::signal::Signal::SIGTERM);
            signal_groups(&groups, nix::sys::signal::Signal::SIGTERM);

            // The CLI's own escalation has to happen here rather than in the
            // runner: if it ignores SIGTERM the runner is still waiting for an
            // exit that has not come.
            tokio::time::sleep(grace).await;
            if !exited.load(Ordering::Acquire) {
                tracing::warn!(pid, "child ignored SIGTERM; sending SIGKILL");
                signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            }
            // The groups are escalated by the runner's teardown, which runs on
            // every exit path and, unlike this task, is awaited before the
            // server is allowed to finish shutting down.
        });
    }

    /// Signal process groups on this child's behalf.
    ///
    /// A detached test handle (pid 0) decides what it *would* signal but never
    /// signals: pid 0 means "our own process group" to `killpg`.
    pub fn signal_groups(&self, groups: &[i32], sig: nix::sys::signal::Signal) {
        if self.pid == 0 {
            tracing::debug!(?groups, ?sig, "detached handle: not signalling");
            return;
        }
        signal_groups(groups, sig);
    }

    /// A handle with no process behind it, for testing the supervisor without
    /// spawning anything. It reports as already exited, so no signal is ever
    /// sent — signalling pid 0 would hit our own process group.
    /// A handle with no process of its own behind it, for testing the
    /// supervisor without spawning a CLI. It reports as already exited.
    ///
    /// pid 0 means "signal nothing" — `killpg(0)` would hit our own process
    /// group. A test that wants the group signalling to actually happen passes
    /// a real pid; it is only ever used as the root of a tree walk and as a
    /// `killpg` target for groups already filtered to what the caller
    /// recorded.
    #[cfg(test)]
    pub fn detached_with_pid(pid: u32) -> (Self, mpsc::UnboundedReceiver<Value>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop_requests = Arc::new(AtomicUsize::new(0));
        (
            Self {
                pid,
                stdin: tx,
                exited: Arc::new(AtomicBool::new(true)),
                stop_requests: stop_requests.clone(),
            },
            rx,
            stop_requests,
        )
    }
}

/// One row of the process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Seconds since the process started, or `None` when `ps` did not say.
    pub elapsed_secs: Option<i64>,
    /// A process that has exited and is waiting to be reaped. It holds nothing
    /// and cannot be signalled to any effect, so it never counts as a group
    /// being alive — otherwise a group would look alive until its parent got
    /// round to `wait`ing.
    pub zombie: bool,
}

impl ProcEntry {
    /// When this process started, in epoch milliseconds, given the time the
    /// table was read. `ps` reports whole seconds, so this is that coarse.
    pub fn started_ms(&self, table_read_ms: i64) -> Option<i64> {
        self.elapsed_secs.map(|secs| table_read_ms - secs * 1000)
    }
}

/// A process group started by one of the agent's tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupRecord {
    pub pgid: i32,
    /// When this group was first observed under the agent, epoch ms.
    pub first_seen_ms: i64,
    /// The group *leader* we saw under the CLI, pinned by pid and start time.
    ///
    /// A descendant may `setpgid(0, G)` itself into any pre-existing group in
    /// its session — the server, its agents and (when launched from a terminal)
    /// the operator's shell all share one session — so "a descendant is in this
    /// group" proves nothing. That the group's *leader* is a descendant does.
    pub witness_pid: i32,
    pub witness_started_ms: i64,
    /// The most recent moment at which this group was *proved* to be the
    /// agent's — by the witness, or by the continuity test below.
    pub proven_ms: i64,
}

/// The process groups an agent's tool calls have started.
///
/// Accumulated while the agent runs, because by the time the CLI is gone its
/// descendants have reparented to init and cannot be found by walking the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    pub groups: Vec<GroupRecord>,
}

/// `ps` reports elapsed time in whole seconds, so a process started in the same
/// second as an observation can look marginally younger than it was.
const CLOCK_SLACK_MS: i64 = 1_500;

/// The most groups we will track. Only ever reached if an agent leaves this
/// many *live* groups behind, which no ordinary session does.
const MAX_GROUPS: usize = 128;

impl Sweep {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn group_ids(&self) -> Vec<i32> {
        self.groups.iter().map(|g| g.pgid).collect()
    }

    fn record(&mut self, pgid: i32, now_ms: i64) -> &mut GroupRecord {
        if let Some(index) = self.groups.iter().position(|g| g.pgid == pgid) {
            return &mut self.groups[index];
        }
        self.groups.push(GroupRecord {
            pgid,
            first_seen_ms: now_ms,
            witness_pid: 0,
            witness_started_ms: 0,
            proven_ms: now_ms,
        });
        let last = self.groups.len() - 1;
        &mut self.groups[last]
    }

    /// Fold a newer observation in. A group already known keeps its earliest
    /// sighting and takes the later of the two proofs.
    pub fn merge(&mut self, other: Sweep) {
        for group in other.groups {
            let existing = self.record(group.pgid, group.first_seen_ms);
            existing.first_seen_ms = existing.first_seen_ms.min(group.first_seen_ms);
            existing.proven_ms = existing.proven_ms.max(group.proven_ms);
            // The first witness is the one we keep: it is the sighting the
            // continuity argument is anchored to.
            if existing.witness_pid == 0 {
                existing.witness_pid = group.witness_pid;
                existing.witness_started_ms = group.witness_started_ms;
            }
        }
    }

    /// Re-prove ownership of every group we can, and record when.
    ///
    /// This is what keeps up with a long-lived process that forks or re-execs:
    /// `npm run dev` replaces itself and spawns workers born after the sighting
    /// that first recorded the group. Each fresh proof moves the group's
    /// `proven_ms` forward, so those workers — which were alive at that moment —
    /// can themselves carry the proof once the original is gone.
    pub fn confirm(&mut self, table: &[ProcEntry], now_ms: i64) {
        for group in &mut self.groups {
            if owns(group, table, now_ms) {
                group.proven_ms = group.proven_ms.max(now_ms);
            }
        }
    }

    /// Drop groups we can no longer prove are ours — which, in practice, means
    /// groups whose processes have all exited.
    ///
    /// Eviction is by that emptiness, never by age: a group recorded early in
    /// the session and still running is exactly the one worth keeping, and an
    /// insertion-order cap threw those away first.
    pub fn prune(&mut self, table: &[ProcEntry], now_ms: i64) {
        self.groups.retain(|group| owns(group, table, now_ms));
        if self.groups.len() > MAX_GROUPS {
            let excess = self.groups.len() - MAX_GROUPS;
            tracing::warn!(
                excess,
                "more live tool-call process groups than we track; dropping the oldest"
            );
            self.groups.drain(..excess);
        }
    }
}

/// Is this group still, provably, one the agent started?
///
/// The proof is continuity, not identity. A live member that started before the
/// last moment we proved this group was the agent's means the group has been
/// non-empty ever since — and a process group id cannot be handed to something
/// else while the group still has members. So it is the same group we walked out
/// of the CLI's subtree, not a recycled id.
///
/// Pid identity deliberately is *not* a proof: pids recycle too, and macOS wraps
/// them at 99998. A group whose members are all younger than our last proof
/// cannot be told apart from a recycled id, so it is refused — leaving a build
/// running is a nuisance, signalling the user's editor is not.
fn owns(group: &GroupRecord, table: &[ProcEntry], now_ms: i64) -> bool {
    // The leader we saw under the CLI, still alive and still the same process:
    // a recycled pid would not also match the start time we recorded.
    let witness_alive = table.iter().any(|e| {
        !e.zombie
            && e.pid == group.witness_pid
            && e.pgid == group.pgid
            && e.started_ms(now_ms)
                .is_some_and(|s| (s - group.witness_started_ms).abs() <= CLOCK_SLACK_MS)
    });
    if witness_alive {
        return true;
    }
    // Or a live member that was already running when we last proved the group
    // was ours, which means it has been non-empty ever since.
    //
    // The slack is *subtracted*: `ps` truncates elapsed seconds, so a process
    // can look up to a second younger than it is. Adding the slack would admit
    // processes that genuinely started after the proof; subtracting it only
    // refuses a few that genuinely predate it.
    table
        .iter()
        .filter(|e| e.pgid == group.pgid && e.pid > 1 && !e.zombie)
        .any(|e| {
            e.started_ms(now_ms)
                .is_some_and(|started| started + CLOCK_SLACK_MS <= group.proven_ms)
        })
}

/// Walk the process tree under `root_pid` and record the groups its descendants
/// belong to.
///
/// Pure, so the walk can be tested without spawning anything. Breadth-first over
/// `ppid` links with a visited set, so a malformed table containing a cycle
/// terminates instead of hanging. pid 1, our own pid and our own process group
/// are never returned — signalling those would take down init or the server.
pub fn sweep_targets(
    table: &[ProcEntry],
    root_pid: i32,
    own_pid: i32,
    own_pgid: i32,
    forbidden: &[i32],
    now_ms: i64,
) -> Sweep {
    let root_pgid = table
        .iter()
        .find(|e| e.pid == root_pid)
        .map(|e| e.pgid)
        .unwrap_or(root_pid);

    let mut children: HashMap<i32, Vec<&ProcEntry>> = HashMap::new();
    for entry in table {
        children.entry(entry.ppid).or_default().push(entry);
    }

    let mut seen: HashSet<i32> = HashSet::from([root_pid]);
    let mut queue: VecDeque<i32> = VecDeque::from([root_pid]);
    let mut sweep = Sweep::default();

    while let Some(pid) = queue.pop_front() {
        let Some(kids) = children.get(&pid) else {
            continue;
        };
        for kid in kids {
            // A cycle, or a process claiming itself as its own parent.
            if !seen.insert(kid.pid) {
                continue;
            }
            queue.push_back(kid.pid);
            if kid.pid <= 1 || kid.pid == own_pid {
                continue;
            }
            let group = kid.pgid;
            if group <= 1
                || group == own_pid
                || group == own_pgid
                || group == root_pgid
                || forbidden.contains(&group)
            {
                continue;
            }
            if kid.zombie {
                continue;
            }
            // Only the group's *leader* being a descendant makes the group the
            // agent's. A descendant that merely joined an existing group proves
            // nothing about that group — and adopting one would hand the agent
            // a way to have the server SIGKILL anything in its session.
            if kid.pid != group {
                continue;
            }
            let Some(started) = kid.started_ms(now_ms) else {
                // Without a start time there is no witness to pin, and every
                // later proof would rest on nothing.
                continue;
            };
            let record = sweep.record(group, now_ms);
            record.first_seen_ms = record.first_seen_ms.min(now_ms);
            record.proven_ms = record.proven_ms.max(now_ms);
            if record.witness_pid == 0 {
                record.witness_pid = kid.pid;
                record.witness_started_ms = started;
            }
        }
    }

    sweep.groups.sort_by_key(|g| g.pgid);
    sweep
}

/// Of the groups we recorded, which can we still prove are ours?
///
/// Every signal goes through here: nothing is ever signalled on the strength of
/// a group id alone.
pub fn groups_to_kill(sweep: &Sweep, table: &[ProcEntry], now_ms: i64) -> Vec<i32> {
    sweep
        .groups
        .iter()
        .filter(|group| owns(group, table, now_ms))
        .map(|group| group.pgid)
        .collect()
}

/// What a stop should signal beyond the CLI's own group: a fresh walk of the
/// tree, folded into what the session has accumulated, and then filtered by the
/// same ownership test everything else uses.
pub fn stop_targets(
    known: &Sweep,
    table: &[ProcEntry],
    root_pid: i32,
    own_pid: i32,
    own_pgid: i32,
    forbidden: &[i32],
    now_ms: i64,
) -> Vec<i32> {
    let mut sweep = sweep_targets(table, root_pid, own_pid, own_pgid, forbidden, now_ms);
    sweep.merge(known.clone());
    groups_to_kill(&sweep, table, now_ms)
}

/// Read the process table. Blocking: call from `spawn_blocking`.
pub fn process_table() -> Vec<ProcEntry> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,etime=,state="])
        .output()
    {
        Ok(output) if output.status.success() => output.stdout,
        Ok(output) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "could not read the process table"
            );
            return Vec::new();
        }
        Err(err) => {
            tracing::warn!(?err, "could not run ps");
            return Vec::new();
        }
    };
    parse_process_table(&String::from_utf8_lossy(&output))
}

/// Parse `ps -axo pid=,ppid=,pgid=,etime=,state=` output. Unparseable lines are
/// skipped.
pub fn parse_process_table(text: &str) -> Vec<ProcEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let pgid = fields.next()?.parse().ok()?;
            let elapsed_secs = fields.next().and_then(parse_etime);
            let zombie = fields.next().is_some_and(|state| state.starts_with('Z'));
            Some(ProcEntry {
                pid,
                ppid,
                pgid,
                elapsed_secs,
                zombie,
            })
        })
        .collect()
}

/// Parse an `etime` field: `MM:SS`, `HH:MM:SS` or `DD-HH:MM:SS`.
pub fn parse_etime(text: &str) -> Option<i64> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.trim().parse::<i64>().ok()?, rest),
        None => (0, text),
    };
    let mut secs = days * 86_400;
    let parts: Vec<&str> = rest.split(':').collect();
    let mut scale = 1;
    for part in parts.iter().rev() {
        if scale > 3600 {
            return None;
        }
        secs += part.trim().parse::<i64>().ok()? * scale;
        scale *= 60;
    }
    Some(secs)
}

/// One snapshot cycle: walk the tree, fold it into what we know, pick up the
/// current members of groups we still own, and drop the ones that are gone.
///
/// Blocking: call from `spawn_blocking`.
pub fn refresh_sweep(mut known: Sweep, root_pid: i32, forbidden: &[i32]) -> Sweep {
    if root_pid <= 0 {
        return known;
    }
    let table = process_table();
    if table.is_empty() {
        return known;
    }
    let now = crate::db::now_ms();
    let own_pid = std::process::id() as i32;
    let own_pgid = nix::unistd::getpgrp().as_raw();
    known.merge(sweep_targets(
        &table, root_pid, own_pid, own_pgid, forbidden, now,
    ));
    known.confirm(&table, now);
    known.prune(&table, now);
    known
}

/// [`groups_to_kill`] against the live process table. Blocking.
pub fn surviving_groups(sweep: &Sweep) -> Vec<i32> {
    groups_to_kill(sweep, &process_table(), crate::db::now_ms())
}

/// [`stop_targets`] against the live process table. Blocking.
pub fn stop_targets_now(known: &Sweep, root_pid: i32, forbidden: &[i32]) -> Vec<i32> {
    if root_pid <= 0 {
        return Vec::new();
    }
    let own_pid = std::process::id() as i32;
    let own_pgid = nix::unistd::getpgrp().as_raw();
    stop_targets(
        known,
        &process_table(),
        root_pid,
        own_pid,
        own_pgid,
        forbidden,
        crate::db::now_ms(),
    )
}

/// Signal a set of process groups. Every caller must have filtered them through
/// [`groups_to_kill`] or [`stop_targets`] first.
pub fn signal_groups(groups: &[i32], sig: nix::sys::signal::Signal) {
    for group in groups {
        signal_group_id(*group, sig);
    }
}

/// Signal a process group by group id, ignoring "already gone".
fn signal_group_id(pgid: i32, sig: nix::sys::signal::Signal) {
    if pgid <= 1 {
        return;
    }
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), sig) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(err) => tracing::warn!(?err, ?sig, pgid, "failed to signal process group"),
    }
}

/// Signal the child's whole process group, falling back to the child alone if
/// it never became a group leader.
fn signal_group(pid: u32, sig: nix::sys::signal::Signal) {
    let target = nix::unistd::Pid::from_raw(pid as i32);
    match nix::sys::signal::killpg(target, sig) {
        Ok(()) => {}
        // ESRCH simply means the group is already gone.
        Err(nix::errno::Errno::ESRCH) => {}
        Err(err) => {
            tracing::debug!(?err, ?sig, "killpg failed; signalling the child directly");
            if let Err(err) = nix::sys::signal::kill(target, sig)
                && err != nix::errno::Errno::ESRCH
            {
                tracing::warn!(?err, ?sig, "failed to signal child");
            }
        }
    }
}

/// Spawn a `claude` child and start pumping its pipes.
///
/// Returns the handle plus the receiver carrying dispatched actions and the
/// eventual exit.
pub fn spawn(config: &SpawnConfig) -> Result<(ChildHandle, mpsc::UnboundedReceiver<ProcessMsg>)> {
    let argv = config.args.to_argv();
    tracing::info!(bin = %config.claude_bin, cwd = %config.cwd.display(), ?argv, "spawning agent");

    let mut child = Command::new(&config.claude_bin)
        .args(&argv)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so stopping the agent also stops whatever its
        // tool calls started.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", config.claude_bin))?;

    let pid = child.id().ok_or_else(|| anyhow!("child has no pid"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("no stderr pipe"))?;

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Value>();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<ProcessMsg>();

    // Writer.
    tokio::spawn(async move {
        while let Some(value) = stdin_rx.recv().await {
            let line = protocol::to_line(&value);
            if let Err(err) = stdin.write_all(line.as_bytes()).await {
                tracing::warn!(?err, "writing to child stdin");
                break;
            }
            if let Err(err) = stdin.flush().await {
                tracing::warn!(?err, "flushing child stdin");
                break;
            }
        }
    });

    // stdout reader.
    let out_tx = msg_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut dispatcher = Dispatcher::new();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    for action in dispatcher.on_stdout(&line) {
                        if out_tx.send(ProcessMsg::Action(action)).is_err() {
                            return;
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(?err, "reading child stdout");
                    break;
                }
            }
        }
        // `system/init` opens a *turn*, not the process, so a session that was
        // launched and stopped without ever being sent a message legitimately
        // has none. It is only evidence of a protocol change when a turn ran.
        if !dispatcher.saw_init() {
            tracing::debug!("child produced no system/init line; it never took a turn");
        }
    });

    // stderr reader.
    let err_tx = msg_tx.clone();
    tokio::spawn(async move {
        let mut dispatcher = Dispatcher::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            for action in dispatcher.on_stderr(&line) {
                if err_tx.send(ProcessMsg::Action(action)).is_err() {
                    return;
                }
            }
        }
    });

    // Exit monitor.
    //
    // The child is reaped as soon as it exits, and stdout is drained only for a
    // bounded moment afterwards. Waiting for stdout to reach EOF first hangs for
    // as long as anything that inherited the pipe keeps it open, and the agent
    // would sit in `Idle` all the while — never reporting the exit, never
    // becoming resumable. See DRAIN_AFTER_EXIT for what was measured about
    // which children actually inherit it.
    let exited = Arc::new(AtomicBool::new(false));
    let exit_flag = exited.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        exit_flag.store(true, Ordering::Release);
        let _ = tokio::time::timeout(DRAIN_AFTER_EXIT, stdout_task).await;
        let info = match status {
            Ok(status) => ExitInfo {
                code: status.code(),
                signal: exit_signal(&status),
                requested: false,
            },
            Err(err) => {
                tracing::warn!(?err, "waiting on child");
                ExitInfo {
                    code: None,
                    signal: None,
                    requested: false,
                }
            }
        };
        let _ = msg_tx.send(ProcessMsg::Exited(info));
    });

    Ok((
        ChildHandle {
            pid,
            stdin: stdin_tx,
            exited,
            stop_requests: Arc::new(AtomicUsize::new(0)),
        },
        msg_rx,
    ))
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// `claude --version`, for the pinned-version check in the risk register.
pub async fn cli_version(bin: &str, cwd: &Path) -> Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("running {bin} --version"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "{bin} --version failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // "2.1.241 (Claude Code)" → "2.1.241"
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or(&text)
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(lines: &[&str]) -> Vec<Action> {
        let mut d = Dispatcher::new();
        lines.iter().flat_map(|l| d.on_stdout(l)).collect()
    }

    fn kinds(actions: &[Action]) -> Vec<EventKind> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Persist { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn init_persists_and_marks_the_agent_ready() {
        let actions =
            dispatch(&[r#"{"type":"system","subtype":"init","session_id":"s1","tools":["Bash"]}"#]);
        assert_eq!(kinds(&actions), vec![EventKind::System]);
        assert!(actions.contains(&Action::SessionId("s1".into())));
        assert!(actions.contains(&Action::Transition(Transition::Initialized)));
    }

    /// Shapes taken verbatim from a session that hit the account's five-hour
    /// limit mid-task.
    #[test]
    fn a_rate_limited_turn_is_reported_as_a_failure() {
        let actions = dispatch(&[
            r#"{"type":"assistant","error":"rate_limit","is_api_error_message":true,"message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"You've hit your session limit \u00b7 resets 7pm (Africa/Johannesburg)"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"You've hit your session limit \u00b7 resets 7pm (Africa/Johannesburg)","total_cost_usd":89.7}"#,
        ]);

        // The model did not say this, so it is not filed as though it had.
        assert_eq!(
            kinds(&actions),
            vec![EventKind::Error, EventKind::Result],
            "an API error message belongs under `error`, not `assistant`"
        );

        // One notice for the turn, not one per synthesised message.
        let notices: Vec<&Action> = actions
            .iter()
            .filter(|a| matches!(a, Action::Notice { .. }))
            .collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let Action::Notice { level, text } = notices[0] else {
            unreachable!()
        };
        assert_eq!(level, "error");
        assert!(text.contains("HTTP 429"), "{text}");
        assert!(
            text.contains("resets 7pm"),
            "the reason has to survive: {text}"
        );

        // The turn is still over, and the agent still goes idle: the process is
        // alive and can be spoken to. Only the silence was the bug.
        assert!(actions.contains(&Action::Transition(Transition::TurnEnded)));
        assert!(actions.contains(&Action::Cost(89.7)));
    }

    /// `subtype` says `success` even on the 429, so nothing may key off it —
    /// and a turn that really did succeed must stay quiet, including one whose
    /// `result` text is a refusal like "/resume isn't available here".
    #[test]
    fn a_successful_turn_raises_no_notice() {
        let actions = dispatch(&[
            r#"{"type":"result","subtype":"success","is_error":false,"result":"/resume isn't available in this environment.","num_turns":0}"#,
        ]);
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Notice { .. })),
            "{actions:?}"
        );
        assert_eq!(kinds(&actions), vec![EventKind::Result]);
    }

    #[test]
    fn assistant_tool_use_persists_twice_and_sets_the_sub_label() {
        let actions = dispatch(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"running"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        ]);
        assert_eq!(
            kinds(&actions),
            vec![EventKind::Assistant, EventKind::ToolUse]
        );
        assert!(actions.contains(&Action::StatusDetail(Some("Bash: cargo test".into()))));
    }

    #[test]
    fn tool_results_are_classified_apart_from_user_messages() {
        let tool = dispatch(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ok"}]}}"#,
        ]);
        assert_eq!(kinds(&tool), vec![EventKind::ToolResult]);

        let plain = dispatch(&[r#"{"type":"user","message":{"role":"user","content":"hello"}}"#]);
        assert_eq!(kinds(&plain), vec![EventKind::User]);
    }

    #[test]
    fn result_ends_the_turn_clears_the_label_and_records_cost() {
        let actions = dispatch(&[
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.5}"#,
        ]);
        assert_eq!(kinds(&actions), vec![EventKind::Result]);
        assert!(actions.contains(&Action::Cost(0.5)));
        assert!(actions.contains(&Action::StatusDetail(None)));
        assert!(actions.contains(&Action::Transition(Transition::TurnEnded)));
    }

    /// The whole point of the fix: neither event may reach the events table.
    /// A heartbeat arrives every 30s and a rate-limit event once per API call,
    /// so persisting them buries the transcript.
    #[test]
    fn progress_and_rate_limit_stay_out_of_the_transcript() {
        let actions = dispatch(&[
            r#"{"type":"tool_progress","tool_use_id":"toolu_1-heartbeat-0","tool_name":"Bash","parent_tool_use_id":"toolu_1","elapsed_time_seconds":30,"heartbeat":true}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        ]);
        assert!(kinds(&actions).is_empty(), "got {actions:?}");
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Unrecognised { .. })),
            "these are recognised events now, not parser gaps"
        );
    }

    /// The ids here are the real shape, captured from `claude` 2.1.246: a
    /// heartbeat's own `tool_use_id` is synthetic and its `parent_tool_use_id`
    /// is the tool actually running. Matching on the raw id would never hit.
    #[test]
    fn a_heartbeat_keeps_the_tool_label_and_adds_the_elapsed_time() {
        let actions = dispatch(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"tool_progress","tool_use_id":"toolu_1-heartbeat-1","tool_name":"Bash","parent_tool_use_id":"toolu_1","elapsed_time_seconds":90,"heartbeat":true}"#,
        ]);
        assert_eq!(
            actions.last(),
            Some(&Action::StatusDetail(Some(
                "Bash: cargo test — 1m30s".into()
            )))
        );
    }

    /// An id we cannot resolve to a recorded tool still reports progress, under
    /// the bare tool name. This is also the shape a heartbeat would take if the
    /// synthetic-id convention ever changed.
    #[test]
    fn an_unresolvable_heartbeat_still_reports_the_elapsed_time() {
        let actions = dispatch(&[
            r#"{"type":"tool_progress","tool_use_id":"toolu_9-heartbeat-0","tool_name":"Grep","parent_tool_use_id":"toolu_1","elapsed_time_seconds":30,"heartbeat":true}"#,
        ]);
        assert_eq!(
            actions,
            vec![Action::StatusDetail(Some("Grep — 30s".into()))]
        );

        let odd = dispatch(&[
            r#"{"type":"tool_progress","tool_use_id":"toolu_9","tool_name":"Grep","parent_tool_use_id":null,"elapsed_time_seconds":30}"#,
        ]);
        assert_eq!(odd, vec![Action::StatusDetail(Some("Grep — 30s".into()))]);
    }

    /// Tool ids do not outlive their turn, so a heartbeat quoting a stale id
    /// must fall back to the bare tool name rather than resurrect an old label.
    #[test]
    fn tool_labels_do_not_survive_the_turn_that_created_them() {
        let actions = dispatch(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
            r#"{"type":"tool_progress","tool_use_id":"toolu_1-heartbeat-0","tool_name":"Bash","parent_tool_use_id":"toolu_1","elapsed_time_seconds":30}"#,
        ]);
        assert_eq!(
            actions.last(),
            Some(&Action::StatusDetail(Some("Bash — 30s".into())))
        );
    }

    #[test]
    fn heartbeat_ids_resolve_to_the_tool_they_are_about() {
        assert_eq!(heartbeat_origin("toolu_01A-heartbeat-0"), Some("toolu_01A"));
        assert_eq!(
            heartbeat_origin("toolu_01A-heartbeat-17"),
            Some("toolu_01A")
        );
        // Not the shape: no claim either way.
        assert_eq!(heartbeat_origin("toolu_01A"), None);
        assert_eq!(heartbeat_origin("toolu_01A-heartbeat-"), None);
        assert_eq!(heartbeat_origin("toolu_01A-heartbeat-x"), None);
        assert_eq!(heartbeat_origin("-heartbeat-0"), None);
    }

    #[test]
    fn a_subagent_retry_becomes_a_notice_not_a_status_label() {
        let actions = dispatch(&[
            r#"{"type":"tool_progress","tool_use_id":"tu_9","tool_name":"Task","parent_tool_use_id":null,"elapsed_time_seconds":0,"subagent_type":"Explore","subagent_retry":{"attempt":2,"max_retries":3,"error_status":529,"error_category":"overloaded"}}"#,
        ]);
        assert_eq!(
            actions,
            vec![Action::Notice {
                level: "warn".into(),
                text: "subagent Explore is retrying (2/3) after HTTP 529".into(),
            }]
        );
    }

    #[test]
    fn a_rate_limit_event_carries_its_windows_through() {
        let actions = dispatch(&[
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.42,"resetsAt":1787745600}}}}"#,
        ]);
        let Some(Action::RateLimit(info)) =
            actions.iter().find(|a| matches!(a, Action::RateLimit(_)))
        else {
            panic!("expected a rate limit action, got {actions:?}");
        };
        assert_eq!(info.unified_windows["five_hour"].utilization, Some(0.42));
    }

    #[test]
    fn elapsed_times_read_as_durations() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(60), "1m00s");
        assert_eq!(fmt_elapsed(3599), "59m59s");
        assert_eq!(fmt_elapsed(3600), "1h00m");
        assert_eq!(fmt_elapsed(7860), "2h11m");
    }

    #[test]
    fn partial_deltas_are_broadcast_but_never_persisted() {
        let actions = dispatch(&[
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"text":"he"}}}"#,
        ]);
        assert!(
            kinds(&actions).is_empty(),
            "deltas must not reach the events table"
        );
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::Partial(_)));
    }

    #[test]
    fn permission_requests_persist_the_normalised_prompt() {
        let actions = dispatch(&[
            r#"{"type":"control_request","request_id":"9","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/x"},"tool_use_id":"tu_2"}}"#,
        ]);
        assert_eq!(kinds(&actions), vec![EventKind::PermissionRequest]);
        assert!(actions.contains(&Action::Transition(Transition::PermissionRequested)));
        let Some(Action::Permission(req)) =
            actions.iter().find(|a| matches!(a, Action::Permission(_)))
        else {
            panic!("expected a permission action");
        };
        assert_eq!(req.request_id, "9");
        assert_eq!(req.tool_name, "Write");

        // The persisted payload is what pending_permissions() reads back.
        let Some(Action::Persist { payload, .. }) = actions.first() else {
            panic!("expected a persist action");
        };
        assert_eq!(payload["request_id"], json!("9"));
    }

    #[test]
    fn other_control_requests_are_persisted_as_system_events() {
        let actions = dispatch(&[
            r#"{"type":"control_request","request_id":"1","request":{"subtype":"mcp_message"}}"#,
        ]);
        assert_eq!(kinds(&actions), vec![EventKind::System]);
        assert!(!actions.iter().any(|a| matches!(a, Action::Permission(_))));
    }

    #[test]
    fn control_responses_are_correlated_by_request_id() {
        let actions = dispatch(&[
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"3","response":{"still_queued":["a"]}}}"#,
        ]);
        let Some(Action::ControlResponse {
            request_id,
            payload,
            is_error,
        }) = actions
            .iter()
            .find(|a| matches!(a, Action::ControlResponse { .. }))
        else {
            panic!("expected a control response action");
        };
        assert_eq!(request_id, "3");
        assert!(!is_error);
        assert_eq!(payload["still_queued"], json!(["a"]));
    }

    #[test]
    fn an_initialize_response_yields_the_command_list() {
        let actions = dispatch(&[
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"1","response":{"commands":[{"name":"/compact","description":"d","argumentHint":"h"}]}}}"#,
        ]);
        let Some(Action::Commands {
            request_id,
            commands,
        }) = actions
            .iter()
            .find(|a| matches!(a, Action::Commands { .. }))
        else {
            panic!("expected commands");
        };
        assert_eq!(request_id.as_deref(), Some("1"));
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "/compact");
    }

    #[test]
    fn unknown_lines_are_surfaced_and_still_stored() {
        let actions = dispatch(&[r#"{"type":"from_the_future","a":1}"#]);
        let Some(Action::Persist { kind, payload }) = actions.first() else {
            panic!("expected a persist action");
        };
        assert_eq!(*kind, EventKind::System);
        assert_eq!(payload["_unrecognised"], json!(true));
        assert_eq!(payload["a"], json!(1), "the raw payload survives");
        assert!(matches!(actions.last(), Some(Action::Unrecognised { .. })));
    }

    #[test]
    fn malformed_lines_become_error_events_not_panics() {
        let actions = dispatch(&["}{ not json"]);
        assert_eq!(kinds(&actions), vec![EventKind::Error]);
    }

    #[test]
    fn blank_lines_are_ignored() {
        assert!(dispatch(&["", "   ", "\t"]).is_empty());
    }

    #[test]
    fn a_full_turn_dispatches_in_order() {
        let mut d = Dispatcher::new();
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/a"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","subtype":"success","total_cost_usd":0.1}"#,
        ];
        let actions: Vec<Action> = lines.iter().flat_map(|l| d.on_stdout(l)).collect();
        assert!(d.saw_init());
        assert_eq!(
            kinds(&actions),
            vec![
                EventKind::System,
                EventKind::Assistant,
                EventKind::ToolUse,
                EventKind::ToolResult,
                EventKind::Assistant,
                EventKind::Result,
            ]
        );
    }

    #[test]
    fn a_missing_init_is_noticed() {
        let mut d = Dispatcher::new();
        d.on_stdout(r#"{"type":"result","subtype":"success"}"#);
        assert!(!d.saw_init());
    }

    #[test]
    fn stderr_lines_are_persisted_verbatim() {
        let mut d = Dispatcher::new();
        let actions = d.on_stderr("warning: something happened");
        assert_eq!(kinds(&actions), vec![EventKind::Stderr]);
        let Some(Action::Persist { payload, .. }) = actions.first() else {
            panic!("expected a persist action");
        };
        assert_eq!(payload["text"], json!("warning: something happened"));
        assert!(d.on_stderr("   ").is_empty());
    }

    // -- the descendant sweep ------------------------------------------------

    const NOW: i64 = 1_000_000_000_000;

    /// A process that started `age_secs` ago.
    fn aged(pid: i32, ppid: i32, pgid: i32, age_secs: i64) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            pgid,
            elapsed_secs: Some(age_secs),
            zombie: false,
        }
    }

    fn zombie(pid: i32, ppid: i32, pgid: i32, age_secs: i64) -> ProcEntry {
        ProcEntry {
            zombie: true,
            ..aged(pid, ppid, pgid, age_secs)
        }
    }

    /// A process one second old — younger than any observation in these tests.
    fn entry(pid: i32, ppid: i32, pgid: i32) -> ProcEntry {
        aged(pid, ppid, pgid, 1)
    }

    fn group(sweep: &Sweep, pgid: i32) -> GroupRecord {
        *sweep
            .groups
            .iter()
            .find(|g| g.pgid == pgid)
            .expect("group should be recorded")
    }

    /// The shape captured live: the CLI leads its own group, but each Bash tool
    /// call sits in a **new** group, unreachable from it.
    fn realistic_table() -> Vec<ProcEntry> {
        vec![
            aged(1, 0, 1, 90_000),    // init
            aged(99, 1, 99, 5_000),   // the terminal that started us
            aged(100, 99, 99, 5_000), // the server, sharing the terminal's group
            aged(200, 100, 200, 600), // claude: its own group, via process_group(0)
            aged(300, 200, 300, 60),  // a Bash tool call: a NEW group
            aged(310, 300, 300, 59),  // cargo build, inside the tool call's group
            aged(311, 310, 300, 58),  // rustc, deeper still
            aged(400, 200, 400, 30),  // a second tool call, another new group
            aged(500, 1, 500, 4_000), // an unrelated process on the machine
        ]
    }

    #[test]
    fn the_sweep_finds_every_group_started_by_a_tool_call() {
        let sweep = sweep_targets(&realistic_table(), 200, 100, 99, &[], NOW);
        assert_eq!(sweep.group_ids(), vec![300, 400]);
        assert_eq!(group(&sweep, 300).first_seen_ms, NOW);
        assert_eq!(group(&sweep, 300).proven_ms, NOW);
    }

    #[test]
    fn the_sweep_never_returns_init_or_the_server_itself() {
        let table = vec![
            entry(1, 0, 1),
            entry(99, 1, 99),
            entry(100, 99, 99),
            entry(200, 100, 200),
            // Pathological rows: a child claiming our group, our pid, and init.
            entry(300, 200, 99),  // the server's own group
            entry(301, 200, 100), // the server's pid as a group
            entry(302, 200, 1),   // init's group
            entry(303, 200, 0),   // no group at all
            entry(304, 200, 304), // a legitimate one, to prove the walk ran
        ];
        let sweep = sweep_targets(&table, 200, 100, 99, &[], NOW);
        assert_eq!(sweep.group_ids(), vec![304]);
        for forbidden in [1, 99, 100, 0] {
            assert!(
                !sweep.group_ids().contains(&forbidden),
                "{forbidden} must never be signalled"
            );
        }
    }

    #[test]
    fn the_sweep_excludes_the_childs_own_group_which_killpg_already_covers() {
        let sweep = sweep_targets(&realistic_table(), 200, 100, 99, &[], NOW);
        assert!(!sweep.group_ids().contains(&200));
    }

    #[test]
    fn the_sweep_terminates_on_a_cycle() {
        let table = vec![
            entry(1, 0, 1),
            entry(100, 1, 100),
            entry(200, 100, 200),
            entry(300, 200, 300),
            entry(400, 500, 400),
            entry(500, 400, 500), // 400 <-> 500
            entry(600, 300, 600),
            entry(300, 600, 300), // and a loop back to 300
        ];
        let sweep = sweep_targets(&table, 200, 100, 100, &[], NOW);
        assert_eq!(sweep.group_ids(), vec![300, 600]);
    }

    #[test]
    fn a_child_with_no_descendants_sweeps_nothing() {
        let table = vec![entry(1, 0, 1), entry(100, 1, 99), entry(200, 100, 200)];
        assert!(sweep_targets(&table, 200, 100, 99, &[], NOW).is_empty());
        assert!(sweep_targets(&table, 9999, 100, 99, &[], NOW).is_empty());
        assert!(sweep_targets(&[], 200, 100, 99, &[], NOW).is_empty());
    }

    #[test]
    fn a_process_that_has_left_the_subtree_is_out_of_reach() {
        // Something already reparented to init is in neither the CLI's group
        // nor its subtree, so no snapshot can see it. This is the documented
        // limitation, asserted so it stays honest: against the real CLI a job
        // backgrounded with `&` reaches this state before our first sample runs,
        // and is not swept.
        let table = vec![
            entry(1, 0, 1),
            entry(100, 1, 99),
            entry(200, 100, 200),
            entry(43060, 1, 43058), // a live capture of a backgrounded job
        ];
        assert!(
            !sweep_targets(&table, 200, 100, 99, &[], NOW)
                .group_ids()
                .contains(&43058)
        );
    }

    // -- ownership: what may and may not be signalled ------------------------

    /// A group whose leader we saw under the CLI a minute ago, and last proved
    /// ours at that moment. The leader has since exited.
    fn observed_group() -> Sweep {
        Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW - 60_000,
                witness_pid: 300,
                witness_started_ms: NOW - 61_000,
                proven_ms: NOW - 60_000,
            }],
        }
    }

    #[test]
    fn a_live_witness_proves_the_group() {
        let table = vec![entry(1, 0, 1), aged(300, 1, 300, 61)];
        assert_eq!(groups_to_kill(&observed_group(), &table, NOW), vec![300]);
    }

    #[test]
    fn a_witness_pid_that_came_back_as_something_else_proves_nothing() {
        // Same pid, wrong age: the leader we recorded is gone and this is a
        // different process wearing its number.
        let table = vec![entry(1, 0, 1), aged(300, 1, 300, 3)];
        assert!(groups_to_kill(&observed_group(), &table, NOW).is_empty());
    }

    #[test]
    fn a_group_holding_a_member_older_than_our_proof_is_ours() {
        // The witness has gone, but this process predates the proof, so the
        // group has been alive continuously ever since and its id cannot have
        // been handed to anything else. `npm run dev &` left behind by a tool
        // call is exactly this shape.
        let table = vec![entry(1, 0, 1), aged(4242, 1, 300, 120)];
        assert_eq!(
            groups_to_kill(&observed_group(), &table, NOW),
            vec![300],
            "a build left running must still be reachable"
        );
    }

    #[test]
    fn a_recycled_group_id_is_never_signalled() {
        // Everything of ours has gone; the id now leads something younger than
        // our last proof. It could be the user's editor, or a database.
        let table = vec![entry(1, 0, 1), aged(300, 1, 300, 5), aged(301, 300, 300, 4)];
        assert!(
            groups_to_kill(&observed_group(), &table, NOW).is_empty(),
            "a group id we cannot prove is ours must not be signalled"
        );
    }

    #[test]
    fn the_clock_slack_is_subtracted_never_added() {
        // `ps` truncates elapsed seconds, so a process can look younger than it
        // is. The slack must not become a licence to signal processes that
        // genuinely started *after* the last proof.
        let sweep = Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW - 10_000,
                witness_pid: 300,
                witness_started_ms: NOW - 11_000,
                proven_ms: NOW - 10_000,
            }],
        };
        // Started one second after the proof: inside the old, additive window.
        let just_after = vec![entry(1, 0, 1), aged(9001, 1, 300, 9)];
        assert!(
            groups_to_kill(&sweep, &just_after, NOW).is_empty(),
            "a process younger than the proof must never carry it"
        );
        // Comfortably older than the proof: allowed.
        let older = vec![entry(1, 0, 1), aged(9001, 1, 300, 20)];
        assert_eq!(groups_to_kill(&sweep, &older, NOW), vec![300]);
    }

    #[test]
    fn a_process_of_unknown_age_proves_nothing() {
        let unknown = ProcEntry {
            pid: 310,
            ppid: 1,
            pgid: 300,
            elapsed_secs: None,
            zombie: false,
        };
        assert!(groups_to_kill(&observed_group(), &[unknown], NOW).is_empty());
    }

    #[test]
    fn re_proving_keeps_forked_workers_reachable_after_their_parent_exits() {
        // While the witness is alive, each refresh re-proves the group and
        // moves the proof forward past workers born since the first sighting.
        let mut sweep = observed_group();
        let table = vec![
            entry(1, 0, 1),
            aged(300, 1, 300, 61), // the witness, still alive
            aged(7001, 300, 300, 30),
            aged(7002, 300, 300, 30),
        ];
        sweep.confirm(&table, NOW);
        assert_eq!(group(&sweep, 300).proven_ms, NOW);

        // The witness exits, leaving only the workers. They predate the latest
        // proof, so the group is still provably ours.
        let later = vec![entry(1, 0, 1), aged(7001, 1, 300, 90)];
        assert_eq!(
            groups_to_kill(&sweep, &later, NOW + 60_000),
            vec![300],
            "workers that outlive their parent must stay reachable"
        );
    }

    #[test]
    fn a_group_we_cannot_prove_is_never_re_proved_into_existence() {
        let mut sweep = observed_group();
        let table = vec![entry(1, 0, 1), aged(300, 1, 300, 5), aged(301, 300, 300, 4)];
        sweep.confirm(&table, NOW);
        assert_eq!(
            group(&sweep, 300).proven_ms,
            NOW - 60_000,
            "a group that cannot be proved must not have its proof advanced"
        );
        assert!(groups_to_kill(&sweep, &table, NOW).is_empty());
    }

    // -- what may be ADOPTED in the first place ------------------------------

    #[test]
    fn a_group_a_descendant_merely_joined_is_never_adopted() {
        // The server never calls setsid, so the server, its agents and the
        // operator's shell share a session — and any process may setpgid itself
        // into any group in its own session. A tool call that joins the
        // operator's editor's group must not make that group the agent's.
        let table = vec![
            aged(1, 0, 1, 90_000),
            aged(100, 1, 99, 5_000),  // the server
            aged(200, 100, 200, 600), // claude
            aged(700, 1, 700, 4_000), // the operator's editor, leading group 700
            aged(701, 700, 700, 4_000),
            aged(301, 200, 700, 2), // a tool call that joined group 700
        ];
        let sweep = sweep_targets(&table, 200, 100, 99, &[], NOW);
        assert!(
            !sweep.group_ids().contains(&700),
            "joining a group must not adopt it: {:?}",
            sweep.group_ids()
        );
        assert!(sweep.is_empty());
        assert!(groups_to_kill(&sweep, &table, NOW).is_empty());
    }

    #[test]
    fn a_sibling_agents_group_is_never_adopted() {
        // Two agents run at once. One must never be able to have the server
        // signal the other's process group.
        // The worst case for the leader rule: a process that IS a group leader
        // and IS in our subtree, but whose group belongs to another agent.
        let table = vec![
            aged(1, 0, 1, 90_000),
            aged(100, 1, 99, 5_000),
            aged(200, 100, 200, 600), // our claude
            aged(250, 200, 250, 600), // a sibling agent's group, in our subtree
            aged(300, 200, 300, 5),   // an ordinary tool call of ours
        ];
        let sweep = sweep_targets(&table, 200, 100, 99, &[250], NOW);
        assert!(
            !sweep.group_ids().contains(&250),
            "no agent may sweep another's process group"
        );
        assert_eq!(
            sweep.group_ids(),
            vec![300],
            "its own tool calls are unaffected"
        );
    }

    #[test]
    fn a_group_without_a_readable_start_time_is_not_adopted() {
        // No start time means no witness to pin, and every later proof would
        // rest on nothing.
        let table = vec![
            aged(1, 0, 1, 9000),
            aged(100, 1, 99, 500),
            aged(200, 100, 200, 60),
            ProcEntry {
                pid: 300,
                ppid: 200,
                pgid: 300,
                elapsed_secs: None,
                zombie: false,
            },
        ];
        assert!(sweep_targets(&table, 200, 100, 99, &[], NOW).is_empty());
    }

    #[test]
    fn the_witness_recorded_is_the_leader_we_saw() {
        let sweep = sweep_targets(&realistic_table(), 200, 100, 99, &[], NOW);
        assert_eq!(group(&sweep, 300).witness_pid, 300);
        assert_eq!(group(&sweep, 300).witness_started_ms, NOW - 60_000);
        assert_eq!(group(&sweep, 400).witness_pid, 400);
    }

    // -- accumulation and eviction ------------------------------------------

    fn recorded(pgid: i32, seen_ms: i64) -> GroupRecord {
        GroupRecord {
            pgid,
            first_seen_ms: seen_ms,
            witness_pid: pgid,
            witness_started_ms: seen_ms,
            proven_ms: seen_ms,
        }
    }

    #[test]
    fn a_live_group_is_never_evicted_however_many_tool_calls_follow() {
        // A group recorded early and still running is exactly the one worth
        // keeping. Insertion-order eviction threw those away first.
        let mut sweep = Sweep::default();
        sweep.merge(Sweep {
            groups: vec![recorded(1001, NOW - 600_000)],
        });

        // 200 later tool calls, each its own group, all of them long gone.
        let mut table = vec![entry(1, 0, 1), aged(1001, 1, 1001, 600)];
        for i in 0..200 {
            let pgid = 5000 + i;
            sweep.merge(Sweep {
                groups: vec![recorded(pgid, NOW - 1000)],
            });
            sweep.prune(&table, NOW);
        }

        assert!(
            sweep.group_ids().contains(&1001),
            "the one group still running must survive 200 later tool calls"
        );
        assert_eq!(sweep.group_ids(), vec![1001]);
        assert_eq!(groups_to_kill(&sweep, &table, NOW), vec![1001]);

        // Once it exits too, it is dropped.
        table.retain(|e| e.pid == 1);
        sweep.prune(&table, NOW);
        assert!(sweep.is_empty());
    }

    #[test]
    fn re_observing_a_group_keeps_its_original_witness_and_takes_the_later_proof() {
        let mut sweep = observed_group();
        sweep.merge(Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW,
                witness_pid: 999,
                witness_started_ms: NOW,
                proven_ms: NOW,
            }],
        });
        let g = group(&sweep, 300);
        assert_eq!(
            g.first_seen_ms,
            NOW - 60_000,
            "the earliest sighting is kept"
        );
        assert_eq!(g.witness_pid, 300, "the original witness anchors the proof");
        assert_eq!(g.proven_ms, NOW, "and the latest proof wins");
    }

    #[test]
    fn tracking_stays_bounded_even_if_everything_stays_alive() {
        let mut sweep = Sweep::default();
        let mut table = vec![entry(1, 0, 1)];
        for i in 0..400 {
            let pgid = 6000 + i;
            sweep.merge(Sweep {
                groups: vec![recorded(pgid, NOW - 10_000)],
            });
            table.push(aged(pgid, 1, pgid, 10));
        }
        sweep.prune(&table, NOW);
        assert!(sweep.groups.len() <= 128, "{}", sweep.groups.len());

        // Re-observing one group forever is a single record, not a growing one.
        let mut one = observed_group();
        for _ in 0..500 {
            one.merge(Sweep {
                groups: vec![recorded(300, NOW)],
            });
        }
        assert_eq!(one.groups.len(), 1);
    }

    // -- what a stop signals -------------------------------------------------

    #[test]
    fn a_stop_never_signals_an_unprovable_group_from_the_accumulated_list() {
        // Session-long bookkeeping: group 45123 is long gone and its id has
        // been recycled onto something of the user's; 400 is a live tool call.
        let known = Sweep {
            groups: vec![
                recorded(45123, NOW - 3_600_000),
                GroupRecord {
                    pgid: 400,
                    first_seen_ms: NOW - 30_000,
                    witness_pid: 400,
                    witness_started_ms: NOW - 30_000,
                    proven_ms: NOW - 30_000,
                },
            ],
        };
        let table = vec![
            aged(1, 0, 1, 90_000),
            aged(100, 1, 99, 5_000),
            aged(200, 100, 200, 600),
            aged(400, 200, 400, 30),      // the live tool call
            aged(45123, 1, 45123, 10),    // the user's editor, on a recycled id
            aged(45200, 45123, 45123, 9), // and its child
        ];

        let targets = stop_targets(&known, &table, 200, 100, 99, &[], NOW);
        assert_eq!(
            targets,
            vec![400],
            "the recycled id must not be signalled by a Stop"
        );
        assert!(!targets.contains(&45123));
    }

    #[test]
    fn a_stop_signals_a_group_the_fresh_walk_finds_even_if_it_was_never_recorded() {
        let table = realistic_table();
        let targets = stop_targets(&Sweep::default(), &table, 200, 100, 99, &[], NOW);
        assert_eq!(targets, vec![300, 400]);
    }

    #[test]
    fn a_stop_never_signals_a_group_a_descendant_merely_joined() {
        let table = vec![
            aged(1, 0, 1, 90_000),
            aged(100, 1, 99, 5_000),
            aged(200, 100, 200, 600),
            aged(700, 1, 700, 4_000), // the operator's editor
            aged(301, 200, 700, 2),   // our tool call, joined to it
        ];
        assert!(stop_targets(&Sweep::default(), &table, 200, 100, 99, &[], NOW).is_empty());
    }

    // -- the process table ---------------------------------------------------

    #[test]
    fn the_process_table_parses_real_ps_output() {
        let text = "    1     0     1 03-07:11:59 Ss\n  195     1   195    06:04:30 S\n42962 42921 42962       12:03 S\n";
        assert_eq!(
            parse_process_table(text),
            vec![
                aged(1, 0, 1, 3 * 86_400 + 7 * 3600 + 11 * 60 + 59),
                aged(195, 1, 195, 6 * 3600 + 4 * 60 + 30),
                aged(42962, 42921, 42962, 12 * 60 + 3),
            ]
        );
    }

    #[test]
    fn etime_parses_every_shape_ps_produces() {
        assert_eq!(parse_etime("00:04"), Some(4));
        assert_eq!(parse_etime("12:03"), Some(723));
        assert_eq!(parse_etime("06:04:30"), Some(21_870));
        assert_eq!(parse_etime("3-07:11:59"), Some(3 * 86_400 + 25_919));
        assert_eq!(parse_etime("nonsense"), None);
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
    }

    #[test]
    fn unparseable_process_table_lines_are_skipped_not_fatal() {
        let text = "PID PPID PGID ELAPSED S\n  1 0 1 00:10 S\nnot a row\n\n  2 1\n  3 1 3 bogus\n";
        let table = parse_process_table(text);
        assert_eq!(table.len(), 2);
        assert_eq!(table[0], aged(1, 0, 1, 10));
        // A row whose elapsed time is unreadable is still a process we know of.
        assert_eq!(table[1].pid, 3);
        assert_eq!(table[1].elapsed_secs, None);
    }

    #[test]
    fn a_refresh_of_a_nonsense_pid_changes_nothing() {
        let known = observed_group();
        assert_eq!(refresh_sweep(known.clone(), 0, &[]), known);
        assert_eq!(refresh_sweep(known.clone(), -1, &[]), known);
        assert!(stop_targets_now(&known, 0, &[]).is_empty());
    }

    /// A stub that backgrounds a process inheriting its stdout and then exits —
    /// the shape of `claude` running a tool call that leaves a build behind.
    /// No `claude`, no network, no ports.
    fn stub_that_leaves_a_process_holding_stdout(dir: &Path) -> Option<String> {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        let path = dir.join("leaky-cli");
        // `sleep` inherits stdout, so the pipe stays open after the stub exits
        // — for far longer than any reasonable wait for the exit.
        std::fs::write(&path, "#!/bin/sh\nsleep 30 &\nexit 7\n").ok()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
        Some(path.to_string_lossy().to_string())
    }

    #[tokio::test]
    async fn an_exit_is_reported_even_when_a_grandchild_holds_the_stdout_pipe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(bin) = stub_that_leaves_a_process_holding_stdout(dir.path()) else {
            return;
        };
        let config = SpawnConfig {
            claude_bin: bin,
            cwd: dir.path().to_path_buf(),
            args: LaunchArgs {
                session_id: "s".into(),
                resume: false,
                permission_mode: crate::agent::state::PermissionMode::Ask,
                model: None,
                effort: None,
                max_budget_usd: None,
                add_dirs: Vec::new(),
            },
        };
        let (handle, mut msgs) = spawn(&config).expect("spawn");

        // Bounded so a regression fails the test instead of hanging it. The
        // grandchild holds the pipe for 30s; the drain window is 500ms, so a
        // working exit path reports in well under a second.
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match msgs.recv().await {
                    Some(ProcessMsg::Exited(info)) => return Some(info),
                    Some(ProcessMsg::Action(_)) => continue,
                    None => return None,
                }
            }
        })
        .await
        .expect("the exit must be reported, not wait on a pipe a grandchild holds open");
        assert_eq!(exit.map(|e| e.code), Some(Some(7)));

        // Tidy up the grandchild: it is still in the stub's process group.
        signal_group(handle.pid, nix::sys::signal::Signal::SIGKILL);
    }

    #[test]
    fn a_zombie_never_makes_a_group_look_alive() {
        // A process that has exited but not been reaped is still listed by
        // `ps`. It holds nothing and cannot be signalled to any effect.
        let sweep = Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW - 60_000,
                witness_pid: 300,
                witness_started_ms: NOW - 61_000,
                proven_ms: NOW - 60_000,
            }],
        };
        let table = vec![entry(1, 0, 1), zombie(300, 1, 300, 61)];
        assert!(
            groups_to_kill(&sweep, &table, NOW).is_empty(),
            "a group holding only a zombie is finished"
        );
        // The same group with a live member is still ours.
        let table = vec![
            entry(1, 0, 1),
            zombie(300, 1, 300, 61),
            aged(301, 1, 300, 120),
        ];
        assert_eq!(groups_to_kill(&sweep, &table, NOW), vec![300]);
    }

    #[test]
    fn a_zombie_leader_is_not_adopted() {
        let table = vec![
            aged(1, 0, 1, 90_000),
            aged(100, 1, 99, 5_000),
            aged(200, 100, 200, 600),
            zombie(300, 200, 300, 5),
        ];
        assert!(sweep_targets(&table, 200, 100, 99, &[], NOW).is_empty());
    }

    #[test]
    fn the_process_state_column_is_read() {
        let table = parse_process_table("1 0 1 00:10 Ss\n2 1 2 00:05 Z+\n3 1 3 00:05 R\n");
        assert!(!table[0].zombie);
        assert!(table[1].zombie, "Z means a zombie");
        assert!(!table[2].zombie);
    }

    #[test]
    fn a_zombie_cannot_carry_the_continuity_proof() {
        // The witness has gone; the only process left in the group predates the
        // proof but has itself exited and not been reaped. A zombie holds
        // nothing and cannot be signalled, so it must not keep the group alive
        // in the continuity branch either.
        let sweep = Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW - 60_000,
                witness_pid: 300,
                witness_started_ms: NOW - 61_000,
                proven_ms: NOW - 60_000,
            }],
        };
        let only_zombie = vec![entry(1, 0, 1), zombie(4242, 1, 300, 120)];
        assert!(
            groups_to_kill(&sweep, &only_zombie, NOW).is_empty(),
            "a zombie must not satisfy the continuity test"
        );

        // A live process of the same age does.
        let alive = vec![entry(1, 0, 1), aged(4242, 1, 300, 120)];
        assert_eq!(groups_to_kill(&sweep, &alive, NOW), vec![300]);
    }
}
