//! Agent status state machine and permission modes.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Where an agent is in its lifecycle.
///
/// `Starting → Idle → Working → Idle …` with [`Status::AwaitingApproval`]
/// branching off `Working`, and `Stopped` / `Failed` as terminal states. The
/// exit code, error text and the live tool sub-label live alongside the status
/// on the agent record rather than inside the enum, because that is how they
/// are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Idle,
    Working,
    AwaitingApproval,
    Stopped,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Idle => "idle",
            Status::Working => "working",
            Status::AwaitingApproval => "awaiting_approval",
            Status::Stopped => "stopped",
            Status::Failed => "failed",
        }
    }

    /// Terminal states are only left by spawning the process again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Stopped | Status::Failed)
    }

    /// True while a child process is expected to be alive.
    pub fn is_live(self) -> bool {
        !self.is_terminal()
    }

    /// Apply a transition. `None` means the transition is not valid from this
    /// state and should be logged and ignored rather than applied.
    pub fn apply(self, t: Transition) -> Option<Status> {
        use Status::*;
        use Transition::*;
        match (self, t) {
            // Spawning always resets the machine, including out of terminal states.
            (_, Spawned) => Some(Starting),
            // Nothing but a fresh spawn moves a terminal agent.
            (Stopped | Failed, _) => None,

            (Starting, Initialized) => Some(Idle),
            (s, Initialized) => Some(s),

            (_, TurnStarted) => Some(match self {
                AwaitingApproval => AwaitingApproval,
                _ => Working,
            }),

            (_, PermissionRequested) => Some(AwaitingApproval),
            (AwaitingApproval, PermissionResolved) => Some(Working),
            (s, PermissionResolved) => Some(s),

            (_, TurnEnded) => Some(Idle),
            (_, Exited) => Some(Stopped),
            (_, Errored) => Some(Failed),
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = UnknownStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "starting" => Ok(Status::Starting),
            "idle" => Ok(Status::Idle),
            "working" => Ok(Status::Working),
            "awaiting_approval" => Ok(Status::AwaitingApproval),
            "stopped" => Ok(Status::Stopped),
            "failed" => Ok(Status::Failed),
            other => Err(UnknownStatus(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown agent status: {0}")]
pub struct UnknownStatus(pub String);

/// The events that drive [`Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// A child process was launched (first launch or resume).
    Spawned,
    /// A `system`/`init` line was seen.
    Initialized,
    /// A user message was handed to the CLI.
    TurnStarted,
    /// A `can_use_tool` control request arrived.
    PermissionRequested,
    /// The pending permission request was answered.
    PermissionResolved,
    /// A `result` line closed the turn.
    TurnEnded,
    /// The process exited on request.
    Exited,
    /// The process died unexpectedly, or could not be started.
    Errored,
}

/// The four per-agent permission modes. Chosen at spawn, and changeable
/// afterwards from the agent's own page (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Ask me — `--permission-mode manual --permission-prompt-tool stdio`.
    Ask,
    /// Accept edits — edits auto-approved, everything else still asks.
    AcceptEdits,
    /// Bypass — `--permission-mode bypassPermissions`.
    Bypass,
    /// Dangerously skip all — `--dangerously-skip-permissions`.
    Dangerous,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Bypass => "bypass",
            PermissionMode::Dangerous => "dangerous",
        }
    }

    /// The CLI flags this mode launches with.
    ///
    /// `--permission-prompt-tool stdio` is passed in every mode, not only the
    /// two that ask. It is what the CLI needs in order to be *able* to reach a
    /// handler, and it is only ever consulted by a mode that checks: verified
    /// against the CLI, a `bypass` or `dangerous` launch carrying it still runs
    /// tools without asking. Without it at launch, tightening the mode later
    /// would hit caveat F3 — `manual` with no handler denies silently — so an
    /// agent started in `bypass` could never be pulled back under review.
    pub fn cli_flags(self) -> Vec<String> {
        let owned = |parts: &[&str]| parts.iter().map(|s| (*s).to_string()).collect();
        let mut args: Vec<String> = match self {
            PermissionMode::Ask => owned(&["--permission-mode", "manual"]),
            PermissionMode::AcceptEdits => owned(&["--permission-mode", "acceptEdits"]),
            PermissionMode::Bypass => owned(&["--permission-mode", "bypassPermissions"]),
            PermissionMode::Dangerous => owned(&["--dangerously-skip-permissions"]),
        };
        args.push("--permission-prompt-tool".to_string());
        args.push("stdio".to_string());
        args
    }

    /// The value a `set_permission_mode` control request would carry, where one
    /// exists. `Dangerous` is a launch flag with no runtime equivalent.
    pub fn control_value(self) -> Option<&'static str> {
        match self {
            PermissionMode::Ask => Some("manual"),
            PermissionMode::AcceptEdits => Some("acceptEdits"),
            PermissionMode::Bypass => Some("bypassPermissions"),
            PermissionMode::Dangerous => None,
        }
    }

    /// How much this mode constrains the agent. Higher is stricter.
    ///
    /// Used to tell a downgrade from an upgrade: relaxing the permission mode
    /// is a security decision and has to be a deliberate operator action, not
    /// something that can be slipped through.
    pub fn strictness(self) -> u8 {
        match self {
            PermissionMode::Ask => 3,
            PermissionMode::AcceptEdits => 2,
            PermissionMode::Bypass => 1,
            PermissionMode::Dangerous => 0,
        }
    }

    /// Would moving from `current` to this mode give the agent more freedom?
    pub fn relaxes(self, current: PermissionMode) -> bool {
        self.strictness() < current.strictness()
    }

}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PermissionMode {
    type Err = UnknownPermissionMode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ask" | "manual" => Ok(PermissionMode::Ask),
            "acceptEdits" | "accept_edits" => Ok(PermissionMode::AcceptEdits),
            "bypass" | "bypassPermissions" => Ok(PermissionMode::Bypass),
            "dangerous" => Ok(PermissionMode::Dangerous),
            other => Err(UnknownPermissionMode(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown permission mode: {0}")]
pub struct UnknownPermissionMode(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_cycle() {
        let s = Status::Starting;
        let s = s.apply(Transition::Initialized).expect("init");
        assert_eq!(s, Status::Idle);
        let s = s.apply(Transition::TurnStarted).expect("turn");
        assert_eq!(s, Status::Working);
        let s = s.apply(Transition::TurnEnded).expect("end");
        assert_eq!(s, Status::Idle);
        let s = s.apply(Transition::TurnStarted).expect("turn 2");
        assert_eq!(s, Status::Working);
    }

    #[test]
    fn approval_branches_off_working_and_returns_to_it() {
        let s = Status::Working
            .apply(Transition::PermissionRequested)
            .expect("request");
        assert_eq!(s, Status::AwaitingApproval);
        let s = s.apply(Transition::PermissionResolved).expect("resolve");
        assert_eq!(s, Status::Working);
    }

    #[test]
    fn a_turn_can_end_while_awaiting_approval() {
        assert_eq!(
            Status::AwaitingApproval.apply(Transition::TurnEnded),
            Some(Status::Idle)
        );
    }

    #[test]
    fn queued_message_does_not_disturb_pending_approval() {
        assert_eq!(
            Status::AwaitingApproval.apply(Transition::TurnStarted),
            Some(Status::AwaitingApproval)
        );
    }

    #[test]
    fn init_before_every_turn_does_not_reset_working() {
        assert_eq!(
            Status::Working.apply(Transition::Initialized),
            Some(Status::Working)
        );
        assert_eq!(
            Status::AwaitingApproval.apply(Transition::Initialized),
            Some(Status::AwaitingApproval)
        );
    }

    #[test]
    fn terminal_states_are_sticky_except_for_a_respawn() {
        for terminal in [Status::Stopped, Status::Failed] {
            assert!(terminal.is_terminal());
            for t in [
                Transition::Initialized,
                Transition::TurnStarted,
                Transition::TurnEnded,
                Transition::PermissionRequested,
                Transition::PermissionResolved,
                Transition::Exited,
                Transition::Errored,
            ] {
                assert_eq!(terminal.apply(t), None, "{terminal:?} {t:?}");
            }
            assert_eq!(terminal.apply(Transition::Spawned), Some(Status::Starting));
        }
    }

    #[test]
    fn any_live_state_can_die() {
        for s in [
            Status::Starting,
            Status::Idle,
            Status::Working,
            Status::AwaitingApproval,
        ] {
            assert!(s.is_live());
            assert_eq!(s.apply(Transition::Exited), Some(Status::Stopped));
            assert_eq!(s.apply(Transition::Errored), Some(Status::Failed));
        }
    }

    #[test]
    fn status_strings_round_trip() {
        for s in [
            Status::Starting,
            Status::Idle,
            Status::Working,
            Status::AwaitingApproval,
            Status::Stopped,
            Status::Failed,
        ] {
            assert_eq!(s.as_str().parse::<Status>().expect("parse"), s);
            let json = serde_json::to_string(&s).expect("serialise");
            assert_eq!(json, format!("\"{}\"", s.as_str()));
        }
        assert!("nonsense".parse::<Status>().is_err());
    }

    #[test]
    fn permission_modes_round_trip_and_map_to_flags() {
        for m in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
            PermissionMode::Dangerous,
        ] {
            assert_eq!(m.as_str().parse::<PermissionMode>().expect("parse"), m);
        }
        assert_eq!(
            PermissionMode::Ask.cli_flags(),
            vec![
                "--permission-mode",
                "manual",
                "--permission-prompt-tool",
                "stdio"
            ]
        );
        assert_eq!(
            PermissionMode::Dangerous.cli_flags(),
            vec![
                "--dangerously-skip-permissions",
                "--permission-prompt-tool",
                "stdio"
            ]
        );
        // Every mode launches able to reach the handler, so an agent can be
        // tightened back under review without being relaunched.
        for m in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
            PermissionMode::Dangerous,
        ] {
            assert!(
                m.cli_flags().windows(2).any(|w| w
                    == ["--permission-prompt-tool".to_string(), "stdio".to_string()]),
                "{m} launches with no permission prompt tool"
            );
        }
        assert_eq!(PermissionMode::Dangerous.control_value(), None);
    }

    #[test]
    fn relaxing_the_permission_mode_is_recognised() {
        use PermissionMode::*;
        assert!(Bypass.relaxes(Ask));
        assert!(Dangerous.relaxes(Bypass));
        assert!(AcceptEdits.relaxes(Ask));
        assert!(!Ask.relaxes(Bypass), "tightening is not relaxing");
        assert!(!Ask.relaxes(Ask), "no change is not relaxing");
        assert!(Ask.strictness() > AcceptEdits.strictness());
        assert!(AcceptEdits.strictness() > Bypass.strictness());
        assert!(Bypass.strictness() > Dangerous.strictness());
    }
}
