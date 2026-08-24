//! Child process lifecycle: spawn, stdin writer, stdout/stderr readers, signals.
//!
//! The interpretation of the CLI's output lives in [`Dispatcher`], which is a
//! pure function from a line of text to a list of [`Action`]s. The transport
//! below only moves bytes, so the protocol handling can be tested against
//! synthetic stdout without a real `claude` binary anywhere near it.

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
    /// The signal goes to the whole process group, not just the CLI: `claude`
    /// spawns its own children for Bash tool calls, and a `cargo build` left
    /// running would hold the worktree open and break the delete path.
    pub fn stop(&self, grace: std::time::Duration) {
        self.stop_requests.fetch_add(1, Ordering::AcqRel);
        if self.has_exited() {
            return;
        }
        signal_group(self.pid, nix::sys::signal::Signal::SIGTERM);
        let pid = self.pid;
        let exited = self.exited.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if !exited.load(Ordering::Acquire) {
                tracing::warn!(pid, "child ignored SIGTERM; sending SIGKILL");
                signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            }
        });
    }

    /// A handle with no process behind it, for testing the supervisor without
    /// spawning anything. It reports as already exited, so no signal is ever
    /// sent — signalling pid 0 would hit our own process group.
    #[cfg(test)]
    pub fn detached() -> (Self, mpsc::UnboundedReceiver<Value>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop_requests = Arc::new(AtomicUsize::new(0));
        (
            Self {
                pid: 0,
                stdin: tx,
                exited: Arc::new(AtomicBool::new(true)),
                stop_requests: stop_requests.clone(),
            },
            rx,
            stop_requests,
        )
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

    // Exit monitor: drains stdout first so the exit is the last message.
    let exited = Arc::new(AtomicBool::new(false));
    let exit_flag = exited.clone();
    tokio::spawn(async move {
        let _ = stdout_task.await;
        let status = child.wait().await;
        exit_flag.store(true, Ordering::Release);
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
}
