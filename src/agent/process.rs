//! Child process lifecycle: spawn, stdin writer, stdout/stderr readers, signals.
//!
//! The interpretation of the CLI's output lives in [`Dispatcher`], which is a
//! pure function from a line of text to a list of [`Action`]s. The transport
//! below only moves bytes, so the protocol handling can be tested against
//! synthetic stdout without a real `claude` binary anywhere near it.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::protocol::{
    self, CliEvent, EventKind, LaunchArgs, PermissionRequest, RateLimitInfo, SlashCommand,
    SystemLine, ToolProgressLine, tool_uses,
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
    /// Backgrounded subagents that have not reported yet, `task_id` → label.
    ///
    /// The CLI hands the parent a tool result the instant a subagent *starts*
    /// and lets the turn finish without waiting for it, so the `result` line
    /// is no longer proof that the agent has nothing left to do. This set is.
    background_tasks: BTreeMap<String, String>,
    /// A turn that ended while [`Dispatcher::background_tasks`] was not empty.
    /// The turn is over; the agent is not idle. The CLI wakes it with a
    /// `task_notification` when the subagent reports, and it speaks again with
    /// no operator input at all — so `idle`, which means "your turn to type",
    /// would be a lie. Held until the last subagent is accounted for.
    deferred_turn_end: bool,
    /// Whether a turn is currently producing output. Only the first line of a
    /// turn raises [`Transition::TurnStarted`], so a turn the *CLI* began —
    /// waking on a subagent's result — moves the agent to `working` exactly
    /// once, rather than on every line.
    turn_open: bool,
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

    /// Fold one backgrounded-subagent lifecycle line into the live set, and say
    /// what that does to the status.
    ///
    /// `background_tasks_changed` carries the whole list and is therefore the
    /// authority; the per-task lines are belt and braces, so that a version of
    /// the CLI that stops sending the list — or a task that starts before we
    /// attach — still leaves the set right.
    fn on_task_line(&mut self, sys: &SystemLine) -> Vec<Action> {
        let was_empty = self.background_tasks.is_empty();
        match sys.subtype.as_deref() {
            Some("background_tasks_changed") => {
                let Some(tasks) = &sys.tasks else {
                    return Vec::new();
                };
                self.background_tasks = tasks
                    .iter()
                    .map(|t| {
                        let label = t
                            .description
                            .clone()
                            .or_else(|| t.task_type.clone())
                            .unwrap_or_else(|| t.task_id.clone());
                        (t.task_id.clone(), label)
                    })
                    .collect();
            }
            Some("task_started") => {
                // Only a *backgrounded* subagent outlives its parent's turn. A
                // synchronous one is already covered — the parent sits in the
                // `Task` tool call until it reports — and tracking it would
                // risk pinning the agent to `working` on a task that has no
                // completion line of its own. Anything not known to be
                // backgrounded is left to `background_tasks_changed`, which is
                // the authority on what is actually running.
                if sys.is_backgrounded != Some(true) {
                    return Vec::new();
                }
                let Some(id) = sys.task_id.clone() else {
                    return Vec::new();
                };
                let label = sys
                    .description
                    .clone()
                    .or_else(|| sys.subagent_type.clone())
                    .unwrap_or_else(|| id.clone());
                self.background_tasks.insert(id, label);
            }
            Some("task_progress") => {
                // The subagent is still going and has said what it is doing.
                // Only a refresh of a label we already hold: a progress line
                // for a task we never saw start is not evidence enough to
                // hold the turn open on.
                let Some(id) = sys.task_id.as_deref() else {
                    return Vec::new();
                };
                let Some(slot) = self.background_tasks.get_mut(id) else {
                    return Vec::new();
                };
                if let Some(what) = sys
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                {
                    *slot = what.to_string();
                }
            }
            Some("task_updated") => {
                let (Some(id), Some(status)) = (sys.task_id.as_deref(), sys.patched_status())
                else {
                    return Vec::new();
                };
                if !protocol::is_terminal_task_status(status) {
                    return Vec::new();
                }
                self.background_tasks.remove(id);
            }
            Some("task_notification") => {
                let Some(id) = sys.task_id.as_deref() else {
                    return Vec::new();
                };
                self.background_tasks.remove(id);
            }
            _ => return Vec::new(),
        }

        if self.background_tasks.is_empty() {
            // Nothing left to wait for. A turn that ended while a subagent was
            // running has been held open until now; close it, so an agent the
            // CLI decides not to wake still comes to rest at `idle`. The wake
            // usually beats the operator to it and puts `working` straight
            // back up.
            if self.deferred_turn_end {
                self.deferred_turn_end = false;
                self.turn_open = false;
                return vec![
                    Action::StatusDetail(None),
                    Action::Transition(Transition::TurnEnded),
                ];
            }
            return Vec::new();
        }
        // Only speak for the status line while the agent has nothing of its own
        // to report; mid-turn, the tool it is running is the better label.
        if self.deferred_turn_end && !was_empty {
            return vec![Action::StatusDetail(Some(self.waiting_label()))];
        }
        Vec::new()
    }

    /// Whose work a tool label describes. A subagent's tool is not the agent's
    /// own, and once the agent's turn is over it is the only thing left to
    /// report — so it keeps the framing the waiting label set, rather than
    /// replacing "waiting on a subagent" with a tool the agent is not running.
    fn delegated_label(&self, label: &str, delegated: bool) -> String {
        match (delegated, self.deferred_turn_end) {
            (true, true) => format!("waiting on subagent: {label}"),
            (true, false) => format!("subagent · {label}"),
            (false, _) => label.to_string(),
        }
    }

    /// "waiting on subagent: Read a.txt" — what the agent is blocked on once
    /// its own turn is over.
    fn waiting_label(&self) -> String {
        let mut names = self.background_tasks.values();
        let count = self.background_tasks.len();
        let first = names.next().map(String::as_str).unwrap_or("a subagent");
        let short: String = first.trim().chars().take(60).collect();
        let ellipsis = if short.len() < first.trim().len() {
            "…"
        } else {
            ""
        };
        if count == 1 {
            format!("waiting on subagent: {short}{ellipsis}")
        } else {
            format!("waiting on {count} subagents: {short}{ellipsis}, …")
        }
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
                if let Some(id) = sys.session_id.clone() {
                    out.push(Action::SessionId(id));
                }
                if sys.subtype.as_deref() == Some("init") {
                    self.saw_init = true;
                    out.push(Action::Transition(Transition::Initialized));
                }
                out.extend(self.on_task_line(&sys));
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
                // A subagent talks on its parent's stream, tagged with the tool
                // call that launched it. That is the subagent's turn, not the
                // parent's.
                let delegated = msg.parent_tool_use_id.is_some();
                // A turn is not always ours to start: the CLI wakes the agent
                // when a backgrounded subagent reports, and it talks with no
                // operator input at all. Whoever began it, output means work.
                if !delegated && !self.turn_open {
                    self.turn_open = true;
                    out.push(Action::Transition(Transition::TurnStarted));
                }
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
                    out.push(Action::StatusDetail(Some(
                        self.delegated_label(&label, delegated),
                    )));
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
                self.turn_open = false;
                // The turn is over, but a backgrounded subagent outlives it:
                // the CLI returns the `Task` tool result as soon as the
                // subagent *starts*, and wakes the agent again when it
                // finishes. Reporting `idle` here tells the operator the agent
                // is waiting on them, when it is waiting on its own subagent.
                if self.background_tasks.is_empty() {
                    out.push(Action::StatusDetail(None));
                    out.push(Action::Transition(Transition::TurnEnded));
                } else {
                    self.deferred_turn_end = true;
                    out.push(Action::StatusDetail(Some(self.waiting_label())));
                }
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

/// How long one process-table read may be shared between sweeps (§4).
///
/// Every sweep that is not a teardown reads through [`shared_process_table`]
/// instead of forking its own `ps`, which collapses the burst that follows each
/// tool call — and, across concurrent agents, a burst per agent — into a single
/// scan. The length is chosen so a shared table's staleness stays inside the
/// slack that is *subtracted* from the proof window ([`PROOF_SLACK_MS`]).
///
/// It is deliberately **not** what gets a discovering sweep a table it has not
/// seen. The TTL runs from the moment a read *completes*, so a sample scheduled
/// at 250ms can land back inside it and be handed the 0ms sample's table; that
/// is what [`shared_process_table_after`] exists for.
pub const SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_millis(250);

/// Slack subtracted from the continuity proof window.
///
/// Two sources of imprecision, both handled the same way. `ps` truncates
/// elapsed seconds, so a process can look up to a second younger than it is; a
/// shared snapshot can additionally be up to [`SNAPSHOT_TTL`] old by the time a
/// sweep reads it. Both are *subtracted* from the window, never added: adding
/// them would admit processes that genuinely started after the last proof,
/// where subtracting only refuses a few that genuinely predate it.
const PROOF_SLACK_MS: i64 = CLOCK_SLACK_MS + SNAPSHOT_TTL.as_millis() as i64;

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
    ///
    /// `now_ms` must be the moment the *table* was read, not the current clock
    /// ([`TableSnapshot`]). That makes this idempotent: confirming twice from
    /// one snapshot writes the same timestamp twice, so a shared table can
    /// never advance the proof further than the observation supports.
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
    // The slack is *subtracted* — see [`PROOF_SLACK_MS`] for what goes into it.
    table
        .iter()
        .filter(|e| e.pgid == group.pgid && e.pid > 1 && !e.zombie)
        .any(|e| {
            e.started_ms(now_ms)
                .is_some_and(|started| started + PROOF_SLACK_MS <= group.proven_ms)
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

/// One process-table read, together with the moment it was taken.
///
/// Everything derived from a table — a process's start time, and the
/// `proven_ms` a successful ownership proof writes back — is computed against
/// `read_ms` rather than the current clock. That is what makes a *shared*
/// table safe:
///
/// * start times do not drift as the snapshot ages, so the witness test stays
///   exact and the continuity test stays anchored to the table's own instant;
/// * `proven_ms` is set with `max(proven_ms, read_ms)`, so two sweeps reading
///   the same snapshot write the same value. **One observation cannot advance
///   the proof twice**, which would manufacture continuity the table never
///   showed.
#[derive(Debug, Clone)]
pub struct TableSnapshot {
    pub table: Arc<Vec<ProcEntry>>,
    /// Epoch ms at which the read completed. Taken *after* `ps` returns, so a
    /// process never looks older than the table can justify.
    pub read_ms: i64,
    /// The same instant on the monotonic clock, for the TTL. A wall clock that
    /// steps backwards must not pin a snapshot forever.
    read_at: Instant,
}

impl TableSnapshot {
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    #[cfg(test)]
    fn for_test(table: Vec<ProcEntry>, read_ms: i64) -> Self {
        Self {
            table: Arc::new(table),
            read_ms,
            read_at: Instant::now(),
        }
    }
}

/// The one shared snapshot. `None` until the first read, and never populated
/// with an empty table — an unreadable `ps` must not be pinned for a TTL.
static SHARED_TABLE: Mutex<Option<TableSnapshot>> = Mutex::new(None);

/// Serialises the tests that *observe* the shared snapshot.
///
/// The snapshot is a singleton, so two tests watching it at once each perturb
/// what the other sees. That is not a hypothetical: those tests used to retry
/// until they got an uninterrupted look, every retry forked another `ps`, and
/// the fork load was enough to push the wall-clock-bounded tests elsewhere in
/// the suite past their deadlines — 9 failures in 40 runs of `agent::`, against
/// 0 in 40 before the snapshot landed. Holding this makes each observation
/// uninterrupted, so the tests can assert outright instead of retrying.
///
/// Only observers need it. Everything that merely *uses* the snapshot is
/// already correct under concurrency; this is about tests that assert on which
/// read they were given.
///
/// A `tokio` mutex so an async observer can hold it across an `.await`; it also
/// does not poison, so a test that panics while holding it fails alone.
#[cfg(test)]
pub(crate) static SNAPSHOT_OBSERVERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take [`SNAPSHOT_OBSERVERS`] from a synchronous test. An async one awaits
/// `SNAPSHOT_OBSERVERS.lock()` instead.
#[cfg(test)]
pub(crate) fn observe_snapshot() -> tokio::sync::MutexGuard<'static, ()> {
    SNAPSHOT_OBSERVERS.blocking_lock()
}

/// Read the process table, reusing a recent read if there is one.
///
/// The lock is deliberately held across the `ps`: when a burst of sweeps
/// arrives together, the first forks `ps` and the rest wait for it and then
/// read its result, rather than each forking their own. That is the whole
/// point — 24 simultaneous scans become one.
///
/// Blocking: call from `spawn_blocking`. Never used by teardown, which reads
/// fresh (§4).
pub fn shared_process_table() -> TableSnapshot {
    shared_process_table_after(i64::MIN)
}

/// As [`shared_process_table`], but never hands back a table the caller has
/// already been given.
///
/// A sweep whose job is to *discover* a group has to see something new, and
/// "the TTL will have expired by now" is not a guarantee. The TTL runs from the
/// moment a read *completes*, so a sample scheduled exactly one TTL after an
/// earlier one lands back inside it by however long that earlier scan took —
/// which is the ordinary case, not a rare one, whenever the earlier sample took
/// a fresh read of its own. It then re-reads the table it already had. That
/// matters most for a long foreground tool call, which has no `tool_result`
/// sweep to fall back on.
///
/// So the caller passes the `read_ms` of the last table it used, and a cached
/// table at or before that watermark forces a fresh read. Concurrent agents
/// still share: whichever of them reads first leaves a table newer than all the
/// others' watermarks, and they take it.
pub fn shared_process_table_after(seen_ms: i64) -> TableSnapshot {
    let mut guard = SHARED_TABLE.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(cached) = guard.as_ref()
        && cached.read_at.elapsed() < SNAPSHOT_TTL
        && cached.read_ms > seen_ms
    {
        return cached.clone();
    }
    let fresh = fresh_process_table();
    if !fresh.is_empty() {
        *guard = Some(fresh.clone());
    }
    fresh
}

/// Read the process table now, bypassing (and not disturbing) the shared
/// snapshot. Blocking: call from `spawn_blocking`.
pub fn fresh_process_table() -> TableSnapshot {
    let table = process_table();
    TableSnapshot {
        table: Arc::new(table),
        read_ms: crate::db::now_ms(),
        read_at: Instant::now(),
    }
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
/// Reads through the *shared* snapshot, so the samples that follow a tool call
/// — and the bursts of several concurrent agents arriving together — cost one
/// `ps` between them rather than one each.
///
/// `seen` is the caller's watermark: the `read_ms` of the last table its sweeps
/// used, updated here. An `establishing` sweep — one whose job is to find a
/// group nobody has recorded yet — refuses a table at or before it and reads a
/// fresh one; the rest take whatever is current.
///
/// Blocking: call from `spawn_blocking`.
pub fn refresh_sweep(
    known: Sweep,
    root_pid: i32,
    forbidden: &[i32],
    seen: &std::sync::atomic::AtomicI64,
    establishing: bool,
) -> Sweep {
    if root_pid <= 0 {
        return known;
    }
    let snapshot = if establishing {
        shared_process_table_after(seen.load(Ordering::Acquire))
    } else {
        shared_process_table()
    };
    seen.fetch_max(snapshot.read_ms, Ordering::AcqRel);
    refresh_sweep_with(known, root_pid, forbidden, &snapshot)
}

/// [`refresh_sweep`] against a table already read. Pure, so the cycle can be
/// tested without spawning anything.
pub fn refresh_sweep_with(
    mut known: Sweep,
    root_pid: i32,
    forbidden: &[i32],
    snapshot: &TableSnapshot,
) -> Sweep {
    if root_pid <= 0 || snapshot.is_empty() {
        return known;
    }
    // The snapshot's own instant, not the current clock: see [`TableSnapshot`].
    // Re-running this against the same snapshot is a no-op.
    let now = snapshot.read_ms;
    let table = snapshot.table.as_slice();
    let own_pid = std::process::id() as i32;
    let own_pgid = nix::unistd::getpgrp().as_raw();
    known.merge(sweep_targets(
        table, root_pid, own_pid, own_pgid, forbidden, now,
    ));
    known.confirm(table, now);
    known.prune(table, now);
    known
}

/// [`groups_to_kill`] against a **freshly read** process table. Blocking.
///
/// Teardown decides what gets SIGKILLed and runs once per agent exit rather
/// than per tool call, so it never reads the shared snapshot (§4).
pub fn surviving_groups(sweep: &Sweep) -> Vec<i32> {
    let snapshot = fresh_process_table();
    groups_to_kill(sweep, &snapshot.table, snapshot.read_ms)
}

/// [`stop_targets`] against a **freshly read** process table. Blocking.
///
/// Fresh for the same reason as [`surviving_groups`].
pub fn stop_targets_now(known: &Sweep, root_pid: i32, forbidden: &[i32]) -> Vec<i32> {
    if root_pid <= 0 {
        return Vec::new();
    }
    let own_pid = std::process::id() as i32;
    let own_pgid = nix::unistd::getpgrp().as_raw();
    let snapshot = fresh_process_table();
    stop_targets(
        known,
        &snapshot.table,
        root_pid,
        own_pid,
        own_pgid,
        forbidden,
        snapshot.read_ms,
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

    // -- backgrounded subagents ---------------------------------------------
    //
    // Shapes taken verbatim from a `--output-format stream-json` run of
    // claude 2.1.251 that launched one subagent. The `Task`/`Agent` tool is
    // asynchronous: the parent gets its tool result ("Async agent launched
    // successfully") the moment the subagent starts, finishes its turn, and is
    // woken by the CLI with a `task_notification` when the subagent reports.

    const AGENT_TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01JX","name":"Agent","input":{"subagent_type":"general-purpose","description":"Read a.txt and report contents","prompt":"read a.txt"}}]}}"#;
    const TASKS_ONE: &str = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a618","task_type":"local_agent","description":"Read a.txt and report contents"}],"session_id":"s1"}"#;
    const TASKS_NONE: &str =
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[],"session_id":"s1"}"#;
    const TASK_STARTED: &str = r#"{"type":"system","subtype":"task_started","task_id":"a618","tool_use_id":"toolu_01JX","description":"Read a.txt and report contents","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":1,"task_type":"local_agent","session_id":"s1"}"#;
    const TURN_OVER: &str = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":3,"total_cost_usd":0.4,"result":"The subagent has been launched and is reading a.txt in the background."}"#;

    #[test]
    fn a_turn_that_ends_with_a_subagent_still_running_is_not_idle() {
        let actions = dispatch(&[AGENT_TOOL_USE, TASKS_ONE, TASK_STARTED, TURN_OVER]);

        // The `result` closes the turn, but the agent has not stopped working
        // and the operator is not being waited on: `idle` would say both.
        assert!(
            !actions.contains(&Action::Transition(Transition::TurnEnded)),
            "a turn with a subagent still running must not report idle: {actions:?}"
        );
        assert!(
            !actions.contains(&Action::StatusDetail(None)),
            "the sub-label must not be cleared while the agent is still waiting"
        );
        assert!(
            actions.contains(&Action::StatusDetail(Some(
                "waiting on subagent: Read a.txt and report contents".into()
            ))),
            "the status has to name what is being waited on: {actions:?}"
        );
        // The cost the turn reported still lands.
        assert!(actions.contains(&Action::Cost(0.4)));
    }

    #[test]
    fn the_agent_comes_to_rest_once_the_last_subagent_reports() {
        let mut d = Dispatcher::new();
        for line in [AGENT_TOOL_USE, TASKS_ONE, TASK_STARTED, TURN_OVER] {
            d.on_stdout(line);
        }

        // The list is the authority: emptied, nothing is left to wait for, and
        // the turn end that was held back finally lands.
        let actions = d.on_stdout(TASKS_NONE);
        assert!(actions.contains(&Action::Transition(Transition::TurnEnded)));
        assert!(actions.contains(&Action::StatusDetail(None)));

        // ...and only once. A second drain has nothing left to close, so it
        // must not push an idle agent through another turn end.
        let again = d.on_stdout(TASKS_NONE);
        assert!(
            !again.contains(&Action::Transition(Transition::TurnEnded)),
            "{again:?}"
        );
        assert!(!again.contains(&Action::StatusDetail(None)), "{again:?}");
    }

    /// The per-task lines close the turn too, for a CLI that stops sending the
    /// list. An agent held at `working` by a task nobody ever retires is worse
    /// than one that goes idle a moment early.
    #[test]
    fn a_completed_task_releases_the_turn_without_the_list() {
        let mut d = Dispatcher::new();
        for line in [AGENT_TOOL_USE, TASK_STARTED, TURN_OVER] {
            d.on_stdout(line);
        }
        let progress = d.on_stdout(
            r#"{"type":"system","subtype":"task_progress","task_id":"a618","description":"Reading a.txt","last_tool_name":"Read","session_id":"s1"}"#,
        );
        assert!(
            progress.contains(&Action::StatusDetail(Some(
                "waiting on subagent: Reading a.txt".into()
            ))),
            "a subagent's own progress is the best label there is: {progress:?}"
        );
        assert!(!progress.contains(&Action::Transition(Transition::TurnEnded)));

        let done = d.on_stdout(
            r#"{"type":"system","subtype":"task_updated","task_id":"a618","patch":{"status":"completed","end_time":1788185758593},"session_id":"s1"}"#,
        );
        assert!(done.contains(&Action::Transition(Transition::TurnEnded)));
        assert!(done.contains(&Action::StatusDetail(None)));
    }

    #[test]
    fn several_subagents_are_counted_and_the_last_one_closes_the_turn() {
        let mut d = Dispatcher::new();
        d.on_stdout(TASKS_ONE);
        d.on_stdout(TURN_OVER);
        let actions = d.on_stdout(
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a618","description":"Read a.txt and report contents"},{"task_id":"b729","description":"Read b.txt"}],"session_id":"s1"}"#,
        );
        let Some(Action::StatusDetail(Some(label))) = actions
            .iter()
            .find(|a| matches!(a, Action::StatusDetail(Some(_))))
            .cloned()
        else {
            panic!("expected a refreshed label: {actions:?}");
        };
        assert!(label.starts_with("waiting on 2 subagents:"), "{label}");
        assert!(
            !actions.contains(&Action::Transition(Transition::TurnEnded)),
            "one of the two is still running"
        );
    }

    /// A subagent's own lines arrive on the parent's stream, tagged with the
    /// tool call that launched it. They are the subagent working, not the
    /// parent starting a turn, and the status has to say whose work it is.
    #[test]
    fn a_subagents_lines_do_not_pass_as_the_parents_own_turn() {
        let mut d = Dispatcher::new();
        for line in [AGENT_TOOL_USE, TASKS_ONE, TASK_STARTED, TURN_OVER] {
            d.on_stdout(line);
        }
        let delegated = d.on_stdout(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_01JX","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_9","name":"Read","input":{"file_path":"a.txt"}}]}}"#,
        );
        assert!(
            !delegated.contains(&Action::Transition(Transition::TurnStarted)),
            "the parent's turn is over; the subagent's is not the parent's: {delegated:?}"
        );
        assert!(
            delegated.contains(&Action::StatusDetail(Some(
                "waiting on subagent: Read: a.txt".into()
            ))),
            "a subagent's tool must not read as one the agent is running: {delegated:?}"
        );

        // The drain closes the turn the subagent's chatter left open, so the
        // parent's wake-up still registers as a turn of its own.
        assert!(
            d.on_stdout(TASKS_NONE)
                .contains(&Action::Transition(Transition::TurnEnded))
        );
        let woken = d.on_stdout(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The subagent found: hello world"}]}}"#,
        );
        assert!(
            woken.contains(&Action::Transition(Transition::TurnStarted)),
            "an idle agent that starts talking is working: {woken:?}"
        );
    }

    /// A subagent the parent waits on inline needs no help from us: the parent
    /// is inside the tool call for its whole life. Holding the turn open for
    /// one risks an agent stuck at `working` on a task with no completion line.
    #[test]
    fn a_synchronous_subagent_does_not_hold_the_turn_open() {
        let mut d = Dispatcher::new();
        d.on_stdout(AGENT_TOOL_USE);
        d.on_stdout(
            r#"{"type":"system","subtype":"task_started","task_id":"c930","description":"Inline work","subagent_type":"general-purpose","is_backgrounded":false,"session_id":"s1"}"#,
        );
        let end = d.on_stdout(TURN_OVER);
        assert!(end.contains(&Action::Transition(Transition::TurnEnded)));
        assert!(end.contains(&Action::StatusDetail(None)));
    }

    /// The wake-up turn arrives with no message from us. Nothing marks its
    /// start but the output itself, so the output has to.
    #[test]
    fn a_turn_the_cli_begins_on_its_own_still_moves_the_agent_to_working() {
        let mut d = Dispatcher::new();
        for line in [
            AGENT_TOOL_USE,
            TASKS_ONE,
            TASK_STARTED,
            TURN_OVER,
            TASKS_NONE,
        ] {
            d.on_stdout(line);
        }
        let woken = d.on_stdout(
            r#"{"type":"system","subtype":"task_notification","task_id":"a618","tool_use_id":"toolu_01JX","status":"completed","summary":"a.txt contains hello world","session_id":"s1"}"#,
        );
        assert!(
            !woken.contains(&Action::Transition(Transition::TurnEnded)),
            "the task was already accounted for by the list: {woken:?}"
        );

        let actions = d.on_stdout(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The subagent found: hello world"}]}}"#,
        );
        assert!(
            actions.contains(&Action::Transition(Transition::TurnStarted)),
            "an assistant line with no message from us is still a turn: {actions:?}"
        );

        // Once per turn, not once per line.
        let more = d.on_stdout(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Anything else?"}]}}"#,
        );
        assert!(!more.contains(&Action::Transition(Transition::TurnStarted)));

        // And with nothing outstanding, the turn ends the ordinary way.
        let end =
            d.on_stdout(r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1}"#);
        assert!(end.contains(&Action::Transition(Transition::TurnEnded)));
        assert!(end.contains(&Action::StatusDetail(None)));
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
        let seen = std::sync::atomic::AtomicI64::new(i64::MIN);
        assert_eq!(refresh_sweep(known.clone(), 0, &[], &seen, true), known);
        assert_eq!(refresh_sweep(known.clone(), -1, &[], &seen, true), known);
        assert!(stop_targets_now(&known, 0, &[]).is_empty());
    }

    // -- the shared snapshot -------------------------------------------------

    /// The invariant a shared table has to carry: **one observation cannot
    /// advance `proven_ms` twice**. Two sweeps reading the same snapshot must
    /// leave the sweep exactly where one did — otherwise the second sweep
    /// manufactures continuity that the table never showed, and a group could
    /// stay "proved" on the strength of a single old look.
    #[test]
    fn a_shared_snapshot_cannot_advance_the_proof_twice() {
        // The tree as it was when the snapshot was taken, one second ago.
        let snapshot = TableSnapshot::for_test(realistic_table(), NOW - 1_000);
        let once = refresh_sweep_with(Sweep::default(), 200, &[], &snapshot);
        let twice = refresh_sweep_with(once.clone(), 200, &[], &snapshot);
        assert_eq!(once, twice, "a re-read of one snapshot must change nothing");
        assert_eq!(
            group(&once, 300).proven_ms,
            NOW - 1_000,
            "the proof is stamped with the moment the table was read, not with now"
        );

        // And a third pass, however much later it happens, still cannot move
        // the proof past what that one observation supports.
        let thrice = refresh_sweep_with(twice, 200, &[], &snapshot);
        assert_eq!(group(&thrice, 300).proven_ms, NOW - 1_000);
    }

    /// A snapshot's staleness lands on the same side as `ps`'s whole-second
    /// truncation: subtracted from the proof window, never added. A process
    /// that could have started after the last proof — anywhere inside the
    /// truncation *or* the TTL — must not carry it.
    #[test]
    fn snapshot_staleness_narrows_the_proof_window_rather_than_widening_it() {
        assert!(
            PROOF_SLACK_MS >= CLOCK_SLACK_MS + SNAPSHOT_TTL.as_millis() as i64,
            "the TTL has to be inside the slack that is subtracted"
        );
        let known = observed_group(); // proven at NOW - 60_000, witness gone
        let secs = PROOF_SLACK_MS / 1_000;

        // A member young enough to fall inside the combined slack is refused:
        // it cannot be told apart from one that started after the proof.
        let inside = vec![entry(1, 0, 1), aged(301, 1, 300, 60 + secs)];
        assert!(
            groups_to_kill(&known, &inside, NOW).is_empty(),
            "a member inside the slack must not carry the proof"
        );

        // Comfortably older than the proof, and it does.
        let outside = vec![entry(1, 0, 1), aged(301, 1, 300, 60 + secs + 2)];
        assert_eq!(groups_to_kill(&known, &outside, NOW), vec![300]);
    }

    /// Sweeps arriving together share one read. This is the whole change: a
    /// burst of six samples per tool call, times however many agents are
    /// running, collapses to a single `ps`.
    #[test]
    fn sweeps_within_the_ttl_share_one_process_table_read() {
        let _observing = observe_snapshot();
        // The first read may hand back a snapshot already near the end of its
        // TTL, so start from one we know is new.
        let first = fresh_and_shared();
        let second = shared_process_table();
        assert!(
            Arc::ptr_eq(&first.table, &second.table),
            "two reads inside the TTL must be the same read"
        );
    }

    /// A snapshot that has just been taken, so a test measuring TTL behaviour
    /// starts from a known age rather than from whatever was left behind.
    fn fresh_and_shared() -> TableSnapshot {
        shared_process_table_after(i64::MAX - 1)
    }

    /// What DESIGN.md §4's cost table was produced with. Ignored by default: it
    /// forks several hundred `ps` processes and takes the best part of a
    /// minute. Re-run it, on an idle machine, whenever the cadence changes:
    ///
    /// ```text
    /// cargo test --release measure_the_sweep_cost -- --ignored --nocapture --test-threads=1
    /// ```
    #[test]
    #[ignore = "a measurement, not a check — see DESIGN.md §4"]
    fn measure_the_sweep_cost() {
        let _observing = observe_snapshot();

        /// CPU consumed by this process *and* by every child it has reaped —
        /// which is where a forked `ps` shows up.
        fn cpu_ms() -> f64 {
            let mut total = 0.0;
            for who in [nix::libc::RUSAGE_SELF, nix::libc::RUSAGE_CHILDREN] {
                let mut usage: nix::libc::rusage = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { nix::libc::getrusage(who, &mut usage) }, 0);
                for time in [usage.ru_utime, usage.ru_stime] {
                    total += time.tv_sec as f64 * 1e3 + time.tv_usec as f64 / 1e3;
                }
            }
            total
        }

        fn measure(label: &str, iters: usize, mut body: impl FnMut()) -> f64 {
            let (cpu, wall) = (cpu_ms(), Instant::now());
            for _ in 0..iters {
                body();
            }
            let per_call = (cpu_ms() - cpu) / iters as f64;
            let wall_per_call = wall.elapsed().as_secs_f64() * 1e3 / iters as f64;
            println!("{label:<46} {per_call:>7.2} ms CPU  {wall_per_call:>8.2} ms wall");
            per_call
        }

        fn run(program: &str, args: &[&str]) {
            let _ = std::process::Command::new(program).args(args).output();
        }

        let table = process_table();
        let own = std::process::id().to_string();
        println!("\nprocesses on this machine: {}\n", table.len());

        println!("-- what one scan costs -------------------------------------");
        measure("bare fork+exec (/usr/bin/true)", 100, || {
            run("/usr/bin/true", &[])
        });
        measure("ps -p self (startup, no full scan)", 100, || {
            run("ps", &["-p", &own, "-o", "pid="])
        });
        let scan = measure("ps -axo (the scan we actually run)", 100, || {
            let _ = process_table();
        });

        // Each tool call is measured starting from an *expired* snapshot, so
        // nothing is credited to a read the previous iteration happened to
        // leave warm.
        let cold = || std::thread::sleep(SNAPSHOT_TTL + std::time::Duration::from_millis(20));

        println!("\n-- what one tool call costs --------------------------------");
        // The cadence before the shared snapshot: six samples over the first
        // quarter-second, then one when the call returns. Seven fresh reads.
        measure("before: 7 fresh reads per tool call", 20, || {
            cold();
            for ms in [0, 8, 20, 45, 90, 250] {
                if ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                let _ = fresh_process_table();
            }
            let _ = fresh_process_table();
        });

        // After. One agent's watermark, exactly as the supervisor keeps it: the
        // discovering sweeps refuse a table they have already been given, the
        // rest take whatever is current.
        let seen = std::sync::atomic::AtomicI64::new(i64::MIN);
        let discover = || {
            let snapshot = shared_process_table_after(seen.load(Ordering::Acquire));
            seen.fetch_max(snapshot.read_ms, Ordering::AcqRel);
        };

        // An agent that *is* tracking a group: every sample runs.
        measure("after, tracking: 6 samples + result", 20, || {
            cold();
            for ms in [0, 8, 20, 45, 90, 250] {
                if ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                if ms == 0 || ms == 250 {
                    discover();
                } else {
                    let _ = shared_process_table();
                }
            }
            discover();
        });

        // And the common case: nothing tracked, so the four gated samples do
        // nothing at all and only the discovering sweeps run.
        measure("after, nothing tracked: 2 samples + result", 20, || {
            cold();
            discover();
            std::thread::sleep(std::time::Duration::from_millis(250));
            discover();
            discover();
        });

        println!("\n-- a burst: one tool call across 4 concurrent agents -------");
        // The whole schedule, on four agents at once — what actually arrives
        // when four agents make a tool call in the same instant.
        for (label, shared) in [("before (a read per sweep)", false), ("after", true)] {
            cold();
            let (cpu, wall) = (cpu_ms(), Instant::now());
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    // One watermark per agent, as the supervisor keeps them.
                    let seen = std::sync::atomic::AtomicI64::new(i64::MIN);
                    scope.spawn(move || {
                        for ms in [0, 8, 20, 45, 90, 250] {
                            if ms > 0 {
                                std::thread::sleep(std::time::Duration::from_millis(ms));
                            }
                            if !shared {
                                let _ = fresh_process_table();
                            } else if ms == 0 || ms == 250 {
                                let snapshot =
                                    shared_process_table_after(seen.load(Ordering::Acquire));
                                seen.fetch_max(snapshot.read_ms, Ordering::AcqRel);
                            } else {
                                let _ = shared_process_table();
                            }
                        }
                        if shared {
                            let snapshot = shared_process_table_after(seen.load(Ordering::Acquire));
                            seen.fetch_max(snapshot.read_ms, Ordering::AcqRel);
                        } else {
                            let _ = fresh_process_table();
                        }
                    });
                }
            });
            println!(
                "{label:<46} {:>7.2} ms CPU  {:>8.2} ms wall",
                cpu_ms() - cpu,
                wall.elapsed().as_secs_f64() * 1e3
            );
        }
        println!("\n(one scan = {scan:.2} ms CPU)\n");
    }

    /// A sweep that has to *discover* something is never handed a table it has
    /// already seen, however recently that table was read. Waiting out the TTL
    /// is not good enough: the TTL runs from the moment a read completes, and
    /// under load a scan takes long enough that a sample scheduled to land past
    /// the TTL lands back inside it.
    #[test]
    fn a_discovering_sweep_refuses_a_table_it_has_already_been_given() {
        let _observing = observe_snapshot();
        let first = fresh_and_shared();
        // Same watermark, well inside the TTL: a plain shared read hands back
        // the identical table, and the discovering read refuses to.
        assert!(Arc::ptr_eq(&shared_process_table().table, &first.table));
        let second = shared_process_table_after(first.read_ms);
        assert!(
            !Arc::ptr_eq(&second.table, &first.table),
            "a table at the watermark must never be handed back"
        );
        assert!(second.read_ms >= first.read_ms);
        // And having taken that one, a caller still behind it may share it.
        assert!(
            Arc::ptr_eq(
                &shared_process_table_after(first.read_ms).table,
                &second.table
            ),
            "a caller still behind the newest read may share it"
        );
    }

    /// Teardown decides what gets SIGKILLed, so it never reads the shared
    /// snapshot — and never disturbs it either, so a fresh read on the way out
    /// cannot leave a stale table behind for the sweeps.
    #[test]
    fn a_fresh_read_neither_uses_nor_replaces_the_shared_snapshot() {
        let _observing = observe_snapshot();
        let before = fresh_and_shared();
        let fresh = fresh_process_table();
        let after = shared_process_table();
        assert!(
            !Arc::ptr_eq(&fresh.table, &before.table),
            "a fresh read must never hand back the cached table"
        );
        assert!(!fresh.is_empty(), "ps should have told us about something");
        assert!(
            Arc::ptr_eq(&before.table, &after.table),
            "a fresh read must not become the cached table"
        );
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
