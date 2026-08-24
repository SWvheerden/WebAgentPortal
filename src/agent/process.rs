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
    self, CliEvent, EventKind, LaunchArgs, PermissionRequest, SlashCommand, tool_uses,
};
use super::state::Transition;

/// How long stdout is drained after the child exits, before the exit is
/// reported. Bounded because a process the CLI left running inherits its stdout
/// pipe and would otherwise hold it open indefinitely.
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
    /// The slash command list from `initialize` (F9).
    Commands(Vec<SlashCommand>),
    /// The session id the CLI reported.
    SessionId(String),
    /// A line we could not classify — logged and surfaced, never fatal.
    Unrecognised { kind: String, reason: String },
}

/// Turns CLI output lines into [`Action`]s. Holds only the small amount of
/// state needed to notice a missing `init`.
#[derive(Debug, Default)]
pub struct Dispatcher {
    saw_init: bool,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once a `system`/`init` line has been seen, which is the startup
    /// assertion from the risk register.
    pub fn saw_init(&self) -> bool {
        self.saw_init
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
            CliEvent::Assistant(msg) => {
                out.push(Action::Persist {
                    kind: EventKind::Assistant,
                    payload: raw,
                });
                for use_ in tool_uses(&msg) {
                    out.push(Action::StatusDetail(Some(use_.label())));
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
                        out.push(Action::Commands(commands));
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
    /// **Not** handled: a process the agent deliberately detached — `nohup … &`,
    /// `setsid`, anything already reparented to pid 1 before the snapshot. It is
    /// in neither the CLI's group nor its subtree, and macOS has no cgroup
    /// equivalent to catch it. See DESIGN.md §4.
    pub fn stop(&self, grace: std::time::Duration, known: Sweep) {
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
            let groups = tokio::task::spawn_blocking(move || stop_targets_now(&known, pid as i32))
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
    /// The most recent moment at which this group was *proved* to be the
    /// agent's — either by the tree walk, or by the continuity test below.
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
    table
        .iter()
        .filter(|e| e.pgid == group.pgid && e.pid > 1)
        .any(|e| {
            e.started_ms(now_ms)
                .is_some_and(|started| started <= group.proven_ms + CLOCK_SLACK_MS)
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
            if group <= 1 || group == own_pid || group == own_pgid || group == root_pgid {
                continue;
            }
            // Found in the CLI's subtree right now: proof enough, and the
            // moment of proof is now.
            let record = sweep.record(group, now_ms);
            record.first_seen_ms = record.first_seen_ms.min(now_ms);
            record.proven_ms = record.proven_ms.max(now_ms);
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
    now_ms: i64,
) -> Vec<i32> {
    let mut sweep = sweep_targets(table, root_pid, own_pid, own_pgid, now_ms);
    sweep.merge(known.clone());
    groups_to_kill(&sweep, table, now_ms)
}

/// Read the process table. Blocking: call from `spawn_blocking`.
pub fn process_table() -> Vec<ProcEntry> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,etime="])
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

/// Parse `ps -axo pid=,ppid=,pgid=,etime=` output. Unparseable lines are skipped.
pub fn parse_process_table(text: &str) -> Vec<ProcEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let pgid = fields.next()?.parse().ok()?;
            let elapsed_secs = fields.next().and_then(parse_etime);
            Some(ProcEntry {
                pid,
                ppid,
                pgid,
                elapsed_secs,
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
pub fn refresh_sweep(mut known: Sweep, root_pid: i32) -> Sweep {
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
    known.merge(sweep_targets(&table, root_pid, own_pid, own_pgid, now));
    known.confirm(&table, now);
    known.prune(&table, now);
    known
}

/// [`groups_to_kill`] against the live process table. Blocking.
pub fn surviving_groups(sweep: &Sweep) -> Vec<i32> {
    groups_to_kill(sweep, &process_table(), crate::db::now_ms())
}

/// [`stop_targets`] against the live process table. Blocking.
pub fn stop_targets_now(known: &Sweep, root_pid: i32) -> Vec<i32> {
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
        if !dispatcher.saw_init() {
            tracing::warn!("child produced no system/init line; protocol may have changed");
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
    // bounded moment afterwards. Waiting for stdout to reach EOF first would
    // hang forever whenever a tool call leaves a process holding the CLI's
    // stdout pipe open — a backgrounded build inherits that pipe — and the
    // agent would sit in `Idle` for as long as that process lived, never
    // reporting the exit and never becoming resumable.
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
        let Some(Action::Commands(cmds)) =
            actions.iter().find(|a| matches!(a, Action::Commands(_)))
        else {
            panic!("expected commands");
        };
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "/compact");
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
        let sweep = sweep_targets(&realistic_table(), 200, 100, 99, NOW);
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
        let sweep = sweep_targets(&table, 200, 100, 99, NOW);
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
        let sweep = sweep_targets(&realistic_table(), 200, 100, 99, NOW);
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
        let sweep = sweep_targets(&table, 200, 100, 100, NOW);
        assert_eq!(sweep.group_ids(), vec![300, 600]);
    }

    #[test]
    fn a_child_with_no_descendants_sweeps_nothing() {
        let table = vec![entry(1, 0, 1), entry(100, 1, 99), entry(200, 100, 200)];
        assert!(sweep_targets(&table, 200, 100, 99, NOW).is_empty());
        assert!(sweep_targets(&table, 9999, 100, 99, NOW).is_empty());
        assert!(sweep_targets(&[], 200, 100, 99, NOW).is_empty());
    }

    #[test]
    fn a_detached_descendant_is_honestly_out_of_reach() {
        // Something already reparented to init is in neither the CLI's group
        // nor its subtree, so a snapshot taken now cannot see it. The early
        // snapshot exists to catch it *before* this happens.
        let table = vec![
            entry(1, 0, 1),
            entry(100, 1, 99),
            entry(200, 100, 200),
            entry(43060, 1, 43058), // the live capture of a `nohup … &`
        ];
        assert!(
            !sweep_targets(&table, 200, 100, 99, NOW)
                .group_ids()
                .contains(&43058)
        );
    }

    // -- ownership: what may and may not be signalled ------------------------

    /// A group observed — and last proved to be ours — one minute ago.
    fn observed_group() -> Sweep {
        Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW - 60_000,
                proven_ms: NOW - 60_000,
            }],
        }
    }

    #[test]
    fn a_group_holding_a_member_older_than_our_proof_is_ours() {
        // The pids we walked may be long gone, but this process predates the
        // proof, so the group has been alive continuously ever since and its id
        // cannot have been handed to anything else. `npm run dev &` left behind
        // by a tool call is exactly this shape, and pid-identity matching
        // dropped it on the floor.
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
    fn a_recycled_pid_does_not_prove_ownership() {
        // The recycled group's leader has the very pid the tree walk recorded a
        // minute ago. Identity would say "ours"; it is not, and only its age
        // gives it away.
        let sweep = Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW - 3_600_000,
                proven_ms: NOW - 3_600_000,
            }],
        };
        let table = vec![entry(1, 0, 1), aged(300, 1, 300, 5)];
        assert!(groups_to_kill(&sweep, &table, NOW).is_empty());
    }

    #[test]
    fn a_group_with_nothing_left_alive_is_not_signalled() {
        assert!(groups_to_kill(&observed_group(), &[entry(1, 0, 1)], NOW).is_empty());
        assert!(groups_to_kill(&Sweep::default(), &realistic_table(), NOW).is_empty());
    }

    #[test]
    fn a_process_of_unknown_age_proves_nothing() {
        // `ps` said nothing useful about elapsed time, so there is no proof to
        // be had and the group is left alone.
        let unknown = ProcEntry {
            pid: 310,
            ppid: 1,
            pgid: 300,
            elapsed_secs: None,
        };
        assert!(groups_to_kill(&observed_group(), &[unknown], NOW).is_empty());
    }

    #[test]
    fn re_proving_keeps_forked_workers_reachable_after_their_parent_exits() {
        // The dev server forks workers born long after the sighting that first
        // recorded the group. While its parent is alive, each refresh re-proves
        // the group and moves the proof forward past them.
        let mut sweep = observed_group();
        let table = vec![
            entry(1, 0, 1),
            aged(310, 1, 300, 59), // the original, older than the proof
            aged(7001, 310, 300, 2),
            aged(7002, 310, 300, 2),
        ];
        sweep.confirm(&table, NOW);
        assert_eq!(group(&sweep, 300).proven_ms, NOW);

        // The parent exits, leaving only the workers. They predate the latest
        // proof, so the group is still provably ours.
        let later = vec![entry(1, 0, 1), aged(7001, 1, 300, 62)];
        assert_eq!(
            groups_to_kill(&sweep, &later, NOW + 60_000),
            vec![300],
            "workers that outlive their parent must stay reachable"
        );
    }

    #[test]
    fn a_group_we_cannot_prove_is_never_re_proved_into_existence() {
        let mut sweep = observed_group();
        // The group id has been recycled: nothing here predates our proof.
        let table = vec![entry(1, 0, 1), aged(300, 1, 300, 5), aged(301, 300, 300, 4)];
        sweep.confirm(&table, NOW);
        assert_eq!(
            group(&sweep, 300).proven_ms,
            NOW - 60_000,
            "a group that cannot be proved must not have its proof advanced"
        );
        assert!(groups_to_kill(&sweep, &table, NOW).is_empty());
    }

    // -- accumulation and eviction ------------------------------------------

    #[test]
    fn a_live_group_is_never_evicted_however_many_tool_calls_follow() {
        // The requirement: a group recorded early and still running is exactly
        // the one worth keeping. Insertion-order eviction threw those away
        // first, which is backwards.
        let mut sweep = Sweep::default();
        sweep.merge(Sweep {
            groups: vec![GroupRecord {
                pgid: 1001,
                first_seen_ms: NOW - 600_000,
                proven_ms: NOW - 600_000,
            }],
        });

        // 200 later tool calls, each its own group, all of them long gone.
        let mut table = vec![entry(1, 0, 1), aged(1001, 1, 1001, 900)];
        for i in 0..200 {
            let pgid = 5000 + i;
            sweep.merge(Sweep {
                groups: vec![GroupRecord {
                    pgid,
                    first_seen_ms: NOW - 1000,
                    proven_ms: NOW - 1000,
                }],
            });
            sweep.prune(&table, NOW);
        }

        assert!(
            sweep.group_ids().contains(&1001),
            "the one group still running must survive {} later tool calls",
            200
        );
        assert_eq!(
            sweep.group_ids(),
            vec![1001],
            "and the groups that have exited must be the ones dropped"
        );

        // It is still reachable, which is the point of keeping it.
        assert_eq!(groups_to_kill(&sweep, &table, NOW), vec![1001]);

        // Once it exits too, it is dropped.
        table.retain(|e| e.pid == 1);
        sweep.prune(&table, NOW);
        assert!(sweep.is_empty());
    }

    #[test]
    fn re_observing_a_group_keeps_its_original_sighting_and_adds_members() {
        let mut sweep = observed_group();
        sweep.merge(Sweep {
            groups: vec![GroupRecord {
                pgid: 300,
                first_seen_ms: NOW, // a later sighting
                proven_ms: NOW,
            }],
        });
        assert_eq!(
            group(&sweep, 300).first_seen_ms,
            NOW - 60_000,
            "the earliest sighting is kept"
        );
        assert_eq!(
            group(&sweep, 300).proven_ms,
            NOW,
            "and the latest proof wins, so newer members can carry it"
        );
    }

    #[test]
    fn tracking_stays_bounded_even_if_everything_stays_alive() {
        let mut sweep = Sweep::default();
        let mut table = vec![entry(1, 0, 1)];
        for i in 0..400 {
            let pgid = 6000 + i;
            sweep.merge(Sweep {
                groups: vec![GroupRecord {
                    pgid,
                    first_seen_ms: NOW - 1000,
                    proven_ms: NOW - 1000,
                }],
            });
            table.push(aged(pgid, 1, pgid, 2000));
        }
        sweep.prune(&table, NOW);
        assert!(sweep.groups.len() <= 128, "{}", sweep.groups.len());

        // Re-observing one group forever is a single record, not a growing one.
        let mut one = observed_group();
        for _ in 0..500 {
            one.merge(Sweep {
                groups: vec![GroupRecord {
                    pgid: 300,
                    first_seen_ms: NOW,
                    proven_ms: NOW,
                }],
            });
        }
        assert_eq!(one.groups.len(), 1);
    }

    // -- what a stop signals -------------------------------------------------

    #[test]
    fn a_stop_never_signals_an_unprovable_group_from_the_accumulated_list() {
        // Session-long bookkeeping: group 300 is long gone and its id has been
        // recycled onto something of the user's; group 400 is a live tool call.
        let known = Sweep {
            groups: vec![
                GroupRecord {
                    pgid: 45123,
                    first_seen_ms: NOW - 3_600_000,
                    proven_ms: NOW - 3_600_000,
                },
                GroupRecord {
                    pgid: 400,
                    first_seen_ms: NOW - 30_000,
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

        let targets = stop_targets(&known, &table, 200, 100, 99, NOW);
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
        let targets = stop_targets(&Sweep::default(), &table, 200, 100, 99, NOW);
        assert_eq!(targets, vec![300, 400]);
    }

    // -- the process table ---------------------------------------------------

    #[test]
    fn the_process_table_parses_real_ps_output() {
        let text = "    1     0     1 03-07:11:59\n  195     1   195    06:04:30\n42962 42921 42962       12:03\n";
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
        let text = "PID PPID PGID ELAPSED\n  1 0 1 00:10\nnot a row\n\n  2 1\n  3 1 3 bogus\n";
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
        assert_eq!(refresh_sweep(known.clone(), 0), known);
        assert_eq!(refresh_sweep(known.clone(), -1), known);
        assert!(stop_targets_now(&known, 0).is_empty());
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
}
