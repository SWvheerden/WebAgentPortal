//! Auto-resume: deciding when the account has tokens again, so an agent the
//! rate limit stopped can be told to carry on (§4).
//!
//! The rule the whole thing turns on: **an agent that ran out of tokens is not
//! waiting for the operator, it is waiting for the clock.** `RateLimited` says
//! so, and the reset time is on the last `rate_limit_event` the CLI sent. What
//! was missing was anyone acting on it — the operator had to come back hours
//! later and type at each stalled agent by hand.
//!
//! The judgement lives here, away from the registry, because it is pure and
//! because it is where every way of being wrong has to be paid for. Two of them
//! matter:
//!
//! - **Evidence that predates the stall is not evidence.** The snapshot is
//!   account-wide and last-writer-wins, it is restored from disk at startup,
//!   and the last event before a limit is routinely `allowed_warning`. A
//!   reading taken before this agent stopped says nothing about whether it may
//!   start again, so it is not allowed to say yes.
//! - **A guess must not be repeated forever, and must not stop either.** Every
//!   nudge is an API call, a transcript row and context in the child, so where
//!   nothing is known the watcher waits, then retries on a widening interval
//!   ([`DENSE_TRIES`] of those), then drops to one probe an hour, and stops
//!   for good at [`MAX_NUDGES`]. It cannot simply give up: the nudges are the
//!   only probe there is, so a watcher that stopped would sleep through the
//!   reset it is waiting for.
//! - **Evidence may bring a nudge forward, never put one off.** It moves the
//!   schedule, floored at [`REARM_FLOOR_MS`]; it does not wind the count back,
//!   which is what leaves [`MAX_NUDGES`] a ceiling.
//!
//! The supervisor does the typing; [`Stall`] is the per-agent bookkeeping it
//! carries between passes.

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

/// How long after the reported reset to wait before believing it.
///
/// The window rolls over on the API's clock, not ours, and being a second early
/// spends a turn to be told `429` again.
pub const RESET_GRACE_MS: i64 = 15_000;

/// How long an agent is left alone when nothing is known about the account.
///
/// Without evidence the first nudge is a guess, and a guess made one tick after
/// the limit landed is the behaviour this feature exists to avoid. Five minutes
/// costs nothing against a window measured in hours, and is usually long enough
/// for some agent's CLI to report a snapshot that settles the question.
pub const PATIENCE_MS: i64 = 5 * 60_000;

/// The wait after the first nudge, doubling from there.
pub const FIRST_BACKOFF_MS: i64 = 60_000;

/// The longest the backoff grows to.
pub const MAX_BACKOFF_MS: i64 = 30 * 60_000;

/// How many nudges the dense phase is worth.
///
/// Six, spread by the backoff over roughly half an hour, after which the
/// watcher drops to [`PROBE_MS`]. The failure this bounds is the one with no
/// natural end: a snapshot nothing refreshes reads the same way forever, and a
/// flat retry interval turns that into hundreds of messages and hundreds of
/// transcript rows over a five-hour window.
///
/// It bounds *guessing*, not resuming — [`Stall::saw`] hands the count back the
/// moment the account is newly shown to have tokens, because that is the
/// refresh whose absence the budget was standing in for. A budget that outlived
/// its evidence would strand the agent it exists to rescue: the window really
/// resets four hours later, the verdict really says so, and nothing is sent.
pub const DENSE_TRIES: u32 = 6;

/// How often the watcher probes once the dense phase is spent.
///
/// It must not stop. **The nudges are the only probe there is**: one either
/// gets through, and the CLI reports a snapshot that says so, or it is refused,
/// and the CLI reports that instead. When every agent on the account is out of
/// tokens — the case this whole feature was built for — nothing else is making
/// the call that would refresh the snapshot, so a watcher that gave up at
/// thirty-six minutes would sleep through the reset it is waiting for. Hourly
/// costs about four extra nudges across a five-hour window and buys the
/// recovery outright.
pub const PROBE_MS: i64 = 60 * 60_000;

/// How far forward evidence may pull the next nudge: no closer than this to the
/// previous one.
///
/// A re-arm is not always the good news it looks like. The snapshot is
/// account-wide and last-writer-wins, so a sibling being served on a window
/// this agent is not on — a different model, `seven_day` against `five_hour` —
/// can flip the verdict back and forth between ticks, and without this each
/// flip would fire a nudge on the spot.
///
/// A ceiling on *acceleration*, never a delay: the schedule evidence arrives
/// into already stands, and evidence takes the sooner of the two. Applied the
/// other way it would invert the whole thing — being told the account has
/// tokens would cost an agent in the dense phase up to 29 minutes it would not
/// have lost by being told nothing at all.
pub const REARM_FLOOR_MS: i64 = 30 * 60_000;

/// The most nudges one stall may ever cost, whatever the verdict does.
///
/// The ceiling nothing can reset — [`Stall::nudges`] only ever goes up — and
/// what makes the acceleration safe to have.
/// Twelve leaves the ordinary five-hour stall — six dense, four probes — well
/// clear of it, so it only bites the pathological case it is there for.
pub const MAX_NUDGES: u32 = 12;

/// How long an agent has to be doing something other than sitting out of tokens
/// before its next stall counts as a new one, with a fresh budget.
///
/// A turn that starts and dies on the same 429 is the same stall continuing, so
/// leaving `RateLimited` cannot by itself hand back the budget — that is the
/// loop the backoff is there to stop. Real work takes longer than this.
pub const PROGRESS_MS: i64 = 5 * 60_000;

/// The `status` that means the account was refused.
const REJECTED: &str = "rejected";

/// What the last usage snapshot says about one stalled agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// There are tokens: this agent can be told to carry on.
    Tokens,
    /// The window that refused it has not reset yet. Nothing to do but wait,
    /// and this is the common answer.
    Held,
    /// Nothing that bears on this agent, either way. Not a licence to retry
    /// freely: [`Stall::due`] makes the caller wait first and gives up in the
    /// end.
    NoEvidence,
}

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

/// What the last snapshot any agent's CLI reported says about an agent that has
/// been out of tokens since `stalled_since_ms`.
///
/// A **rejection** is read for its reset time, and its age does not matter: the
/// reset is an absolute instant, so an old refusal whose window has since
/// rolled over is still telling the truth. A rejection carrying no reset time
/// at all tells us nothing.
///
/// Anything else — `allowed`, `allowed_warning` — means only that somebody was
/// served *at the moment it was taken*, which is why it has to be newer than
/// the stall to count. The account is shared: a snapshot restored from disk at
/// startup, one left behind by the turn that ran just before the limit, or one
/// written by a sibling agent's earlier call would otherwise read as "there are
/// tokens" on the first tick after the limit landed.
pub fn verdict(
    snapshot: Option<(i64, &RateLimitInfo)>,
    stalled_since_ms: i64,
    now_ms: i64,
) -> Verdict {
    let Some((captured_at, info)) = snapshot else {
        return Verdict::NoEvidence;
    };
    if info.status == REJECTED {
        return match resets_at_ms(info) {
            // A window that had already rolled over before this agent stopped
            // cannot be the one that stopped it: the reading belongs to an
            // older episode, and "that window has reset" is then a fact about
            // the past rather than a licence to start again.
            Some(at) if at <= stalled_since_ms => Verdict::NoEvidence,
            Some(at) if now_ms >= at + RESET_GRACE_MS => Verdict::Tokens,
            Some(_) => Verdict::Held,
            None => Verdict::NoEvidence,
        };
    }
    if captured_at >= stalled_since_ms {
        Verdict::Tokens
    } else {
        Verdict::NoEvidence
    }
}

/// One agent's stall, as the watcher remembers it between passes.
///
/// Created when the agent is first seen out of tokens, and thrown away once it
/// has been doing something else for [`PROGRESS_MS`] — or the moment it stops
/// running, since a dead agent cannot be resumed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    /// When it was first seen out of tokens in this episode. What a snapshot's
    /// `captured_at` is measured against.
    pub since_ms: i64,
    /// Nudges sent in this episode. One counter, incremented and never
    /// decremented or reset: it decides the phase, the backoff and the ceiling
    /// alike. Evidence moves the *schedule*, not this — a count that could be
    /// wound back would be no ceiling at all.
    pub nudges: u32,
    /// When the last one was sent. Meaningless while `nudges == 0`.
    pub last_ms: i64,
    /// When it was last seen alive but *not* out of tokens, if it currently is.
    /// The episode ends once that has held for [`PROGRESS_MS`].
    pub working_since_ms: Option<i64>,
    /// Whether the previous pass's verdict was [`Verdict::Tokens`], so that
    /// *becoming* so can be told from having been so for hours. Only the
    /// transition is news; a verdict that has read the same all along is the
    /// very thing the backoff is counting.
    pub saw_tokens: bool,
    /// Evidence has arrived since the last nudge, so the next one may come
    /// forward to [`REARM_FLOOR_MS`]. Spent by the nudge it brings forward,
    /// because a single piece of news is worth one nudge, not a standing
    /// licence.
    pub rearmed: bool,
}

impl Stall {
    /// An agent seen out of tokens for the first time.
    pub fn new(now_ms: i64) -> Self {
        Self {
            since_ms: now_ms,
            nudges: 0,
            last_ms: 0,
            working_since_ms: None,
            saw_tokens: false,
            rearmed: false,
        }
    }

    /// Take this pass's verdict, re-arming the budget when the account has
    /// newly been shown to have tokens.
    ///
    /// The budget bounds guessing, and what it stands in for is a reading that
    /// nothing refreshes. A verdict that has just turned to [`Verdict::Tokens`]
    /// *is* that refresh — a snapshot newer than the stall, or the reset time of
    /// the refusal that caused it finally passing — so the count it was keeping
    /// is spent, and starts again.
    ///
    /// This cannot spin. Tokens that turn out not to be there produce a 429,
    /// whose event carries a fresh reset time; the verdict goes
    /// [`Verdict::Held`] and nothing more is sent until the API's own clock
    /// says otherwise. And a verdict that stays `Tokens` does not re-arm
    /// anything: only the transition does, so each re-arming needs a real call
    /// that really was served.
    ///
    /// `Held → Tokens` re-arms for the same reason `NoEvidence → Tokens` does,
    /// and must: a stall that spent its guesses before the CLI ever reported a
    /// reset time would otherwise be stranded by the very reading that settles
    /// the question.
    ///
    /// What a re-arm cannot do is make a stall cost more than [`MAX_NUDGES`].
    /// `NoEvidence → Tokens` involves no clock at all — a sibling served on a
    /// window this agent is not on flips it, and can flip it back next tick —
    /// so "every re-arm needs a real window to pass" holds for `Held → Tokens`
    /// and fails here. The ceiling and [`REARM_FLOOR_MS`] stand in for it.
    pub fn saw(&mut self, verdict: Verdict) {
        let tokens = verdict == Verdict::Tokens;
        if tokens && !self.saw_tokens {
            self.rearmed = true;
        }
        self.saw_tokens = tokens;
    }

    /// Is a nudge due?
    ///
    /// Three phases, and one ceiling over all of them:
    ///
    /// - **dense**, [`DENSE_TRIES`] nudges on the widening backoff. `evidence`
    ///   — the account is known to have tokens, rather than merely not known to
    ///   lack them — makes the first immediate; without it the agent is left
    ///   alone for [`PATIENCE_MS`] first.
    /// - **probe**, once that is spent: one an hour, because the nudges are the
    ///   only thing that can refresh a snapshot nobody else is refreshing.
    /// - **stopped**, at [`MAX_NUDGES`], which no verdict can undo.
    ///
    /// Evidence ([`Stall::saw`]) can pull the next nudge forward to
    /// [`REARM_FLOOR_MS`] after the last one, which is what rescues a stall
    /// sitting in the hourly phase when its window finally resets. It can only
    /// ever bring a nudge forward — never put one off — and the floor is what
    /// keeps a flapping snapshot from nudging every tick.
    pub fn due(&self, now_ms: i64, evidence: bool) -> bool {
        if self.stopped() {
            return false;
        }
        let scheduled = if self.nudges == 0 {
            self.since_ms + if evidence { 0 } else { PATIENCE_MS }
        } else if self.probing() {
            self.last_ms + PROBE_MS
        } else {
            self.last_ms + backoff_ms(self.nudges)
        };
        // Evidence brings the next nudge *forward*, and the floor bounds how
        // far. A `max` here would be an inversion: news that the account has
        // tokens would cost the agent the difference between the floor and a
        // dense backoff of one to sixteen minutes, so it would resume later for
        // having been told than for having been left ignorant.
        let earliest = if self.rearmed && self.nudges > 0 {
            scheduled.min(self.last_ms + REARM_FLOOR_MS)
        } else {
            scheduled
        };
        now_ms >= earliest
    }

    /// Is the dense phase spent, so that the next nudge is an hourly probe?
    /// The operator has to be able to tell "still watching, slowly" from
    /// "given up". One-way, like the counter behind it.
    pub fn probing(&self) -> bool {
        self.nudges >= DENSE_TRIES
    }

    /// Has this stall spent every nudge it will ever get?
    pub fn stopped(&self) -> bool {
        self.nudges >= MAX_NUDGES
    }

    /// Note that a nudge was sent.
    pub fn nudged(&mut self, now_ms: i64) {
        self.nudges += 1;
        self.last_ms = now_ms;
        self.working_since_ms = None;
        // The news has been acted on. Leaving it standing would let one served
        // sibling call buy every subsequent nudge its 30-minute spacing.
        self.rearmed = false;
    }

    /// Note that the agent is out of tokens on this pass.
    pub fn stalled(&mut self) {
        self.working_since_ms = None;
    }

    /// Note that the agent is alive and not out of tokens on this pass.
    ///
    /// Answers whether the episode is over — whether it has been that way long
    /// enough to count as having got somewhere, so the next stall starts with a
    /// fresh budget. A turn that starts and dies on the same 429 does not.
    pub fn working(&mut self, now_ms: i64) -> bool {
        let since = *self.working_since_ms.get_or_insert(now_ms);
        now_ms - since >= PROGRESS_MS
    }
}

/// The wait owed after `tries` nudges: one minute, then two, four, eight …
/// capped at [`MAX_BACKOFF_MS`].
pub fn backoff_ms(tries: u32) -> i64 {
    if tries == 0 {
        return 0;
    }
    FIRST_BACKOFF_MS
        .saturating_mul(1i64 << (tries - 1).min(20))
        .min(MAX_BACKOFF_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::protocol::RateLimitWindow;
    use std::collections::BTreeMap;

    const STALL: i64 = 1_787_800_000_000;

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

    fn allowed() -> RateLimitInfo {
        RateLimitInfo {
            status: "allowed".to_string(),
            ..rejected(None)
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
        let at = |now| verdict(Some((STALL, &info)), STALL, now);
        assert_eq!(at(reset_ms - 60_000), Verdict::Held);
        assert_eq!(
            at(reset_ms),
            Verdict::Held,
            "the window rolls over on the API's clock, not ours"
        );
        assert_eq!(at(reset_ms + RESET_GRACE_MS), Verdict::Tokens);
    }

    /// A refusal is read for its reset time, and that is an absolute instant —
    /// so how old the *reading* is does not matter, only where its window falls
    /// either side of the stall.
    #[test]
    fn a_refusal_is_judged_by_its_window_and_not_by_its_age() {
        // Taken hours before the stall, but its window reset after the agent
        // stopped: that is the ordinary case, seen late.
        let info = rejected(Some((STALL + 3_600_000) / 1_000));
        let old_reading = Some((STALL - 86_400_000, &info));
        assert_eq!(verdict(old_reading, STALL, STALL), Verdict::Held);
        assert_eq!(
            verdict(old_reading, STALL, STALL + 3_700_000),
            Verdict::Tokens
        );

        // A window that had already rolled over when the agent stopped is from
        // an older episode and cannot explain this stall — otherwise a snapshot
        // restored from yesterday resumes the agent on the first tick.
        let stale = rejected(Some((STALL - 3_600_000) / 1_000));
        assert_eq!(
            verdict(Some((STALL - 86_400_000, &stale)), STALL, STALL + 1_000),
            Verdict::NoEvidence
        );
    }

    /// The bug this closes: the last snapshot before a limit is routinely
    /// `allowed_warning`, the store is account-wide and last-writer-wins, and a
    /// restored one can be hours old. Any of those would otherwise read as
    /// "there are tokens" on the first tick after the agent stopped.
    #[test]
    fn a_reading_taken_before_the_stall_is_not_evidence_that_it_is_over() {
        for status in ["allowed", "allowed_warning"] {
            let info = RateLimitInfo {
                status: status.to_string(),
                ..allowed()
            };
            assert_eq!(
                verdict(Some((STALL - 1, &info)), STALL, STALL + 60_000),
                Verdict::NoEvidence,
                "{status} taken before the stall proves nothing about after it"
            );
            assert_eq!(
                verdict(Some((STALL, &info)), STALL, STALL + 60_000),
                Verdict::Tokens,
                "{status} taken since the stall means somebody was served"
            );
        }
    }

    #[test]
    fn nothing_known_is_nothing_claimed() {
        assert_eq!(verdict(None, STALL, STALL), Verdict::NoEvidence);
        assert_eq!(
            verdict(Some((STALL, &rejected(None))), STALL, STALL),
            Verdict::NoEvidence,
            "refused, with no word on when that ends"
        );
    }

    /// With evidence the first nudge is immediate; without it the agent is left
    /// alone first, because a guess one tick after the limit landed is the
    /// behaviour the whole feature exists to avoid.
    #[test]
    fn a_guess_waits_where_evidence_does_not_have_to() {
        let stall = Stall::new(STALL);
        assert!(stall.due(STALL, true));
        assert!(!stall.due(STALL, false));
        assert!(!stall.due(STALL + PATIENCE_MS - 1, false));
        assert!(stall.due(STALL + PATIENCE_MS, false));
    }

    /// Five hours of ticks against a verdict that never changes: the dense
    /// phase, then hourly probes, and never the ~300 messages, transcript rows
    /// and API calls a flat one-a-minute retry would have cost.
    ///
    /// It must not stop altogether. The nudges are the only probe there is —
    /// when every agent on the account is out of tokens, nothing else is making
    /// the call that would refresh the snapshot — so a watcher that gave up at
    /// thirty-six minutes would sleep through the reset it is waiting for.
    #[test]
    fn guessing_widens_into_an_hourly_probe() {
        let mut stall = Stall::new(STALL);
        let mut now = STALL;
        let mut sent = Vec::new();
        // Five hours of ticks, which is a whole rate-limit window.
        while now < STALL + 5 * 3_600_000 {
            if stall.due(now, false) {
                stall.nudged(now);
                sent.push(now - STALL);
            }
            now += TICK.as_millis() as i64;
        }
        // Patience, then 1m, 2m, 4m, 8m, 16m — the dense phase, all of it
        // inside the first hour.
        let dense = &sent[..DENSE_TRIES as usize];
        assert_eq!(dense[0], PATIENCE_MS);
        assert!(
            dense.last().copied().expect("dense") < 3_600_000,
            "{sent:?}"
        );
        for pair in dense.windows(2) {
            assert!(pair[1] - pair[0] >= FIRST_BACKOFF_MS, "{sent:?}");
        }
        // Then one an hour for the rest of the window — four of them, not
        // silence, and nowhere near the ceiling.
        let probes = &sent[DENSE_TRIES as usize..];
        assert_eq!(probes.len(), 4, "sent at {sent:?}");
        for pair in sent[DENSE_TRIES as usize - 1..].windows(2) {
            assert!(pair[1] - pair[0] >= PROBE_MS, "{sent:?}");
        }
        assert!(sent.len() < MAX_NUDGES as usize, "{sent:?}");
        assert!(stall.probing() && !stall.stopped());
    }

    /// The ceiling exists because the re-arm has no clock behind it. A sibling
    /// being served on a window this agent is not on flips the account-wide
    /// snapshot between `allowed` and this agent's own refusal, and every flip
    /// hands the budget back — which without a ceiling is the flat one-a-minute
    /// retry again, by a third route.
    #[test]
    fn a_flapping_verdict_cannot_nudge_forever() {
        let mut stall = Stall::new(STALL);
        let mut now = STALL;
        let mut sent = Vec::new();
        let mut tokens = true;
        // A day of it, so the ceiling is reached rather than merely approached.
        while now < STALL + 24 * 3_600_000 {
            // The worst case: it alternates on every single tick.
            tokens = !tokens;
            let verdict = if tokens {
                Verdict::Tokens
            } else {
                Verdict::NoEvidence
            };
            stall.saw(verdict);
            if stall.due(now, tokens) {
                stall.nudged(now);
                sent.push(now - STALL);
            }
            now += TICK.as_millis() as i64;
        }
        // The ceiling ends it: twelve, and no more, however long this goes on.
        assert_eq!(sent.len(), MAX_NUDGES as usize, "sent at {sent:?}");
        assert!(stall.stopped());
        // Flapping buys nothing the dense backoff would not have given anyway —
        // it moves the schedule, it does not replace it — and once the dense
        // phase is spent the floor holds it to one nudge per half hour rather
        // than the one per tick it would otherwise be.
        for pair in sent.windows(2) {
            assert!(pair[1] - pair[0] >= FIRST_BACKOFF_MS, "{sent:?}");
        }
        for pair in sent[DENSE_TRIES as usize - 1..].windows(2) {
            assert!(pair[1] - pair[0] >= REARM_FLOOR_MS, "{sent:?}");
        }
        stall.saw(Verdict::NoEvidence);
        stall.saw(Verdict::Tokens);
        assert!(
            !stall.due(now, true),
            "the ceiling is the one thing no verdict undoes"
        );
    }

    /// The floor keeps a single flip from firing on the spot, so the ceiling is
    /// not burned through in one burst. What it may never do is *delay*: it
    /// bounds how far forward evidence pulls a nudge, and the schedule it
    /// arrives into still stands.
    #[test]
    fn the_floor_bounds_the_acceleration_and_nothing_else() {
        // In the hourly phase it is the floor that decides, and it is the
        // faster of the two.
        let mut stall = Stall::new(STALL);
        for _ in 0..DENSE_TRIES {
            stall.nudged(STALL);
        }
        stall.saw(Verdict::Tokens);
        assert!(!stall.due(STALL + REARM_FLOOR_MS - 1, true));
        assert!(
            stall.due(STALL + REARM_FLOOR_MS, true),
            "half an hour, not an"
        );
        // Or the "acceleration" would be a delay.
        const { assert!(REARM_FLOOR_MS < PROBE_MS) };

        // In the dense phase the backoff is the faster of the two, so evidence
        // changes nothing at all — rather than costing the agent the difference.
        let mut stall = Stall::new(STALL);
        stall.nudged(STALL);
        let quiet = stall;
        stall.saw(Verdict::Tokens);
        assert!(!stall.due(STALL + FIRST_BACKOFF_MS - 1, true));
        assert!(stall.due(STALL + FIRST_BACKOFF_MS, true));
        assert!(quiet.due(STALL + FIRST_BACKOFF_MS, false), "and the same");

        // The first nudge of a stall has no floor: there is nothing to measure
        // one from.
        let fresh = Stall::new(STALL);
        assert!(fresh.due(STALL, true));
    }

    /// The invariant, stated over every state a stall can be in: being told the
    /// account has tokens never makes the next nudge *later* than being told
    /// nothing would have. Anything else is an inversion — the agent resumes
    /// sooner for having been left ignorant.
    #[test]
    fn evidence_never_delays_a_nudge() {
        let earliest = |stall: &Stall, evidence: bool| {
            (0..=24 * 60)
                .map(|m| STALL + m * 60_000)
                .find(|t| stall.due(*t, evidence))
        };
        for nudges in 0..MAX_NUDGES {
            let mut quiet = Stall::new(STALL);
            quiet.nudges = nudges;
            quiet.last_ms = if nudges == 0 { 0 } else { STALL };
            let mut told = quiet;
            told.saw(Verdict::Tokens);
            let (quiet_at, told_at) = (earliest(&quiet, false), earliest(&told, true));
            assert!(
                told_at <= quiet_at,
                "after {nudges} nudges: told {told_at:?}, ignorant {quiet_at:?}"
            );
        }
    }

    /// The budget bounds guessing, so evidence has to hand it back. Without
    /// this the single-agent case fails exactly where it must not: nothing else
    /// is running to refresh the account-wide snapshot, the six guesses go in
    /// the first half hour, and when the window really does reset four hours
    /// later the verdict says `Tokens`, `evidence` is true — and the agent is
    /// never spoken to again.
    #[test]
    fn evidence_brings_the_rescue_forward() {
        let mut stall = Stall::new(STALL);
        for _ in 0..DENSE_TRIES {
            stall.nudged(STALL);
        }
        assert!(stall.probing(), "the dense phase is spent");
        assert!(
            !stall.due(STALL + PROBE_MS - 1, true),
            "and an hour has not passed"
        );

        // Four hours later a snapshot newer than the stall arrives: the wait
        // drops from the hour to the floor.
        stall.saw(Verdict::Tokens);
        assert!(stall.due(STALL + REARM_FLOOR_MS, true), "and it is rescued");

        // One piece of news is worth one nudge. The nudge it bought spends it,
        // so the hourly schedule resumes rather than every tick being 30
        // minutes from the last.
        stall.nudged(STALL + REARM_FLOOR_MS);
        stall.saw(Verdict::Tokens);
        let next = STALL + REARM_FLOOR_MS;
        assert!(!stall.due(next + REARM_FLOOR_MS, true), "not news any more");
        assert!(stall.due(next + PROBE_MS, true));

        // A stall that spent its guesses before the CLI ever named a reset time
        // is rescued by the same rule the moment that window passes.
        let mut stall = Stall::new(STALL);
        for _ in 0..DENSE_TRIES {
            stall.nudged(STALL);
        }
        stall.saw(Verdict::Held);
        assert!(
            !stall.due(STALL + PROBE_MS - 1, false),
            "held, and not yet due to probe"
        );
        stall.saw(Verdict::Tokens);
        assert!(stall.due(STALL + REARM_FLOOR_MS, true));
    }

    #[test]
    fn the_backoff_doubles_up_to_the_cap() {
        assert_eq!(backoff_ms(0), 0);
        assert_eq!(backoff_ms(1), FIRST_BACKOFF_MS);
        assert_eq!(backoff_ms(2), 2 * FIRST_BACKOFF_MS);
        assert_eq!(backoff_ms(3), 4 * FIRST_BACKOFF_MS);
        assert_eq!(backoff_ms(60), MAX_BACKOFF_MS, "no overflow, no wraparound");
    }

    /// The spacing has to survive the agent leaving `RateLimited`, because that
    /// is exactly what a nudge does: the resumed turn goes `Working` and can be
    /// refused again seconds later. If leaving the state handed back the budget,
    /// the backoff would never apply to the case it exists for.
    #[test]
    fn a_turn_that_starts_and_dies_again_is_the_same_stall() {
        let mut stall = Stall::new(STALL);
        stall.nudged(STALL);
        // The resumed turn ran for a tick, then hit the same limit.
        assert!(
            !stall.working(STALL + 30_000),
            "half a minute of Working is not progress"
        );
        stall.stalled();
        assert!(
            !stall.due(STALL + 31_000, true),
            "the nudge that caused this still counts"
        );
        assert!(stall.due(STALL + FIRST_BACKOFF_MS, true));

        // Real work, on the other hand, ends the episode and the caller drops
        // the record — the next stall starts from nothing. It has to be *seen*
        // working throughout, which is what the passes in between are: a single
        // late look cannot tell an agent that worked all morning from one that
        // started a second ago.
        let mut stall = Stall::new(STALL);
        stall.nudged(STALL);
        assert!(
            !stall.working(STALL + 1_000),
            "the first look starts the clock"
        );
        assert!(!stall.working(STALL + PROGRESS_MS));
        assert!(stall.working(STALL + 1_000 + PROGRESS_MS));
    }
}
