//! Auto-resume: deciding when the account has tokens again, so an agent the
//! rate limit stopped can be told to carry on (§4).
//!
//! The rule the whole thing turns on: **an agent that ran out of tokens is not
//! waiting for the operator, it is waiting for the clock.** `RateLimited` says
//! so, and the reset time is already on the last `rate_limit_event` the CLI
//! sent. What was missing was anyone acting on it — the operator had to come
//! back hours later and type at each stalled agent by hand.
//!
//! The judgement lives here, away from the registry, because it is pure: given
//! the last usage snapshot and the time, are there tokens? The supervisor does
//! the typing.

use std::time::Duration;

use super::protocol::RateLimitInfo;

/// What is typed at an agent the limit stopped.
///
/// Deliberately the same words an operator would use, and deliberately not a
/// slash command: it has to mean "carry on with the task you were given", which
/// only the transcript the agent already holds can supply.
pub const RESUME_PROMPT: &str = "resume where you left off";

/// How often the watcher looks.
///
/// A window reset is not a deadline anyone is racing, and the alternative —
/// arming a timer on each snapshot — buys seconds at the price of a second
/// piece of state that can go stale.
pub const TICK: Duration = Duration::from_secs(30);

/// The soonest an agent may be auto-resumed again after it was.
///
/// This is the loop-stopper. A snapshot whose reset time has passed but which
/// nothing has refreshed reads as "there are tokens" forever, so the retry it
/// licenses has to be bounded to one a minute per agent. A retry costs one API
/// call that fails fast, and a real 429 replaces the snapshot with a fresh
/// reset time, which ends the retrying by itself.
pub const MIN_SPACING_MS: i64 = 60_000;

/// How long after the reported reset to wait before believing it.
///
/// The window rolls over on the API's clock, not ours, and being a second early
/// spends a turn to be told `429` again.
pub const RESET_GRACE_MS: i64 = 15_000;

/// The `status` that means the account was refused.
const REJECTED: &str = "rejected";

/// When the governing window resets, in unix **millis**.
///
/// `resets_at` is the CLI's own answer and is used whenever it is there. The
/// fallback is the *earliest* window reset it reported: which window did the
/// refusing is not knowable from the snapshot, and the earliest is the one that
/// might already have rolled over — being wrong that way costs a retry, being
/// wrong the other way costs hours of a stopped agent.
pub fn resets_at_ms(info: &RateLimitInfo) -> Option<i64> {
    info.resets_at
        .or_else(|| {
            info.unified_windows
                .values()
                .filter_map(|w| w.resets_at)
                .min()
        })
        .map(|seconds| seconds * 1_000)
}

/// Does the account have tokens again, judged from the last snapshot any
/// agent's CLI reported?
///
/// - Not a rejection: somebody was served, so there are tokens. This is also
///   how an account that came back early is noticed — another agent's turn
///   refreshes the snapshot for all of them.
/// - A rejection whose window has since reset: the clock says so.
/// - A rejection still inside its window: no, and this is the common answer.
/// - No snapshot at all: unknowable, so it answers yes and lets the caller's
///   spacing bound the retries. The alternative is waiting on an event that
///   cannot arrive — every agent being out of tokens means no agent is making
///   the API call that would produce one.
pub fn tokens_available(snapshot: Option<&RateLimitInfo>, now_ms: i64) -> bool {
    let Some(info) = snapshot else {
        return true;
    };
    if info.status != REJECTED {
        return true;
    }
    match resets_at_ms(info) {
        Some(at) => now_ms >= at + RESET_GRACE_MS,
        None => true,
    }
}

/// May this agent be auto-resumed now, given when it last was?
pub fn may_resume(last_attempt_ms: Option<i64>, now_ms: i64) -> bool {
    last_attempt_ms.is_none_or(|last| now_ms - last >= MIN_SPACING_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::protocol::RateLimitWindow;
    use std::collections::BTreeMap;

    fn rejected(resets_at: Option<i64>) -> RateLimitInfo {
        RateLimitInfo {
            status: REJECTED.to_string(),
            resets_at,
            rate_limit_type: Some("five_hour".to_string()),
            utilization: Some(1.0),
            is_using_overage: None,
            unified_windows: BTreeMap::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// Unix seconds on the wire, millis everywhere in this process. Getting
    /// that wrong puts every reset in 1970 and resumes instantly, forever.
    #[test]
    fn the_reset_time_is_read_in_seconds_and_answered_in_millis() {
        assert_eq!(
            resets_at_ms(&rejected(Some(1_787_846_400))),
            Some(1_787_846_400_000)
        );
    }

    /// The CLI's own field wins; the windows are only consulted when it is
    /// absent, and then the earliest one is taken — a retry is cheap, hours of
    /// a stopped agent are not.
    #[test]
    fn the_windows_are_a_fallback_and_the_earliest_is_taken() {
        let mut info = rejected(None);
        assert_eq!(resets_at_ms(&info), None, "nothing to go on");

        info.unified_windows = BTreeMap::from([
            (
                "seven_day".to_string(),
                RateLimitWindow {
                    utilization: Some(0.8),
                    resets_at: Some(1_788_217_200),
                },
            ),
            (
                "five_hour".to_string(),
                RateLimitWindow {
                    utilization: Some(1.0),
                    resets_at: Some(1_787_745_600),
                },
            ),
        ]);
        assert_eq!(resets_at_ms(&info), Some(1_787_745_600_000));

        info.resets_at = Some(1_787_700_000);
        assert_eq!(
            resets_at_ms(&info),
            Some(1_787_700_000_000),
            "the governing window the CLI names is not second-guessed"
        );
    }

    #[test]
    fn a_rejection_holds_until_its_window_resets() {
        let info = rejected(Some(1_787_846_400));
        let reset_ms = 1_787_846_400_000;
        assert!(!tokens_available(Some(&info), reset_ms - 60_000));
        assert!(
            !tokens_available(Some(&info), reset_ms),
            "the window rolls over on the API's clock, not ours"
        );
        assert!(tokens_available(Some(&info), reset_ms + RESET_GRACE_MS));
    }

    /// Any snapshot that is not a refusal means somebody was served — which is
    /// how an account that came back early, or was never refused at all, is
    /// noticed without waiting out a stale reset time.
    #[test]
    fn a_snapshot_that_is_not_a_refusal_means_there_are_tokens() {
        for status in ["allowed", "allowed_warning"] {
            let info = RateLimitInfo {
                status: status.to_string(),
                ..rejected(Some(1_787_846_400))
            };
            assert!(
                tokens_available(Some(&info), 1_787_800_000_000),
                "{status} is not a refusal"
            );
        }
    }

    /// Both unknowns answer yes, because the cost of being wrong is one API
    /// call that fails fast, and the cost of the other answer is an agent that
    /// never restarts. The spacing is what keeps that honest.
    #[test]
    fn what_is_not_known_is_retried_rather_than_waited_out() {
        assert!(tokens_available(None, 1_787_800_000_000));
        assert!(tokens_available(Some(&rejected(None)), 1_787_800_000_000));
    }

    #[test]
    fn an_agent_is_not_resumed_twice_in_a_minute() {
        let now = 1_787_800_000_000;
        assert!(may_resume(None, now), "never tried is always allowed");
        assert!(!may_resume(Some(now - 1_000), now));
        assert!(!may_resume(Some(now - MIN_SPACING_MS + 1), now));
        assert!(may_resume(Some(now - MIN_SPACING_MS), now));
    }
}
