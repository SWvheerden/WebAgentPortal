---
name: new-feat
description: Orchestrates coder, reviewer, and (optionally) security-reviewer subagents to implement a fix or feature end-to-end, looping review cycles until clean. Only invoke this explicitly when the user runs /new-feat or asks by name — do not auto-trigger on generic "fix this" or "implement X" requests, since this is a heavyweight, deliberate workflow the user chooses to run.
argument-hint: [task description]
---

# New Feature Orchestrator

You are the orchestrator and verifier for the following **TASK**:

```
$ARGUMENTS
```

Report to the user as each step and each cycle completes, naming the model each subagent used.

## Subagents

`coder` and `reviewer` are labels for your own tracking and reports — they are not registered
agent types. Spawn each as a `general-purpose` agent with the model the user chose.

- **coder** — spawn it once, in Step 1, and keep its ID. Every later fix round goes to that
  same agent via `SendMessage`, so it keeps its context of the change it is making.
- **reviewer** — spawn a fresh one for every review round, so it reads the code cold rather
  than trusting what it approved last round.

**Cycle cap**: if a loop in Step 3 or Step 5 runs 5 cycles without coming back clean, stop
and ask the user how to proceed, listing the findings still open.

## Step 0 — Settle choices, branch, and index before spawning anything

### 0a — Ask the user

Ask these in a single `AskUserQuestion` call and wait for the answers:

1. **Coder model** — `opus` (Recommended), `sonnet`, `haiku`.
2. **Reviewer model** — `opus` (Recommended), `sonnet`, `haiku`.
3. **Security review** after the coder/reviewer loop is clean — `Yes, opus` (Recommended),
   `Yes, sonnet`, `No`. The model chosen here is used by `security-analysis` and every
   sub-agent it spawns.

Refer to the answers below as `$CODER_MODEL`, `$REVIEWER_MODEL`, `$RUN_SECURITY_REVIEW`,
and `$SECURITY_MODEL`.

### 0b — Branch and base ref

- If the working tree has uncommitted changes, ask the user whether to include them, stash
  them, or stop. Coder commits must contain only this TASK's work.
- If the current branch is the default branch, create a feature branch named after the TASK
  (e.g. `feat/<short-slug>` or `fix/<short-slug>`) and switch to it.
- Record `$BASE` = `git merge-base HEAD <default-branch>`. Every review below is scoped to
  `git diff $BASE..HEAD`.

### 0c — Index once

Index the codebase yourself with the `cocoindex-code:ccc` skill and wait for it to finish.
Every subagent below searches that index; they do not re-index. If indexing fails, say so
and continue — subagents fall back to grep and direct file reads.

## Step 1 — Coder

Spawn `coder` on `$CODER_MODEL` with these instructions:

```
** Codebase search
The codebase is already indexed with the cocoindex-code:ccc skill. Search that index to find
files, callers, and related code — do not re-index.

** Style
When implementing the task, keep the following in mind:
* Keep it simple.
* Avoid clever abstractions: do not create custom helper classes or abstract factory patterns if a basic loop or built-in function will work.
* Use straightforward, readable code with no advanced metaprogramming or complex design patterns.

** Task
Implement the TASK at hand:
$ARGUMENTS

Add or update tests covering the new behaviour or the fixed bug.

** Afterwards
Make sure the following commands pass:
* cargo ci-test
* cargo ci-clippy
* cargo +nightly fmt --all

If those commands are not available in this project (not a Rust project, or the
`ci-test` / `ci-clippy` cargo aliases are not defined), fall back to running the
project's own test, lint, and format commands instead.

Then make a commit on the current branch.

** Report back
- What you changed and why, per file.
- The commit hash.
- Each command you ran and whether it passed. If one failed and you could not fix it,
  say so and paste the relevant output.
```

## Step 2 — Reviewer

Spawn a fresh `reviewer` on `$REVIEWER_MODEL` with these instructions, filling in `$BASE`:

```
Do a code review of `git diff $BASE..HEAD` against this TASK:
$ARGUMENTS

The codebase is already indexed with the cocoindex-code:ccc skill — search it to follow
callers and related code outside the diff. Do not re-index. Review only: do not edit,
stage, or commit anything.

Report back only medium and higher issues — ignore all low and coding nits, only focus on
actual issues. Also check that the diff fully implements the TASK; anything missing is an
issue.

The checklist below is written for Rust. For code in other languages, apply the
equivalent concern where one exists and skip the rest.

1. Safety and Soundness (Highest Priority)
- Every unsafe block must have a // SAFETY: comment explaining why it is sound.
- Undefined Behaviour: check raw pointer casting, transmute calls, and UnsafeCell usage for potential data races.
- FFI Boundaries: memory allocated by C is freed by C; Rust memory is not passed to foreign code without #[no_mangle] or proper layout (#[repr(C)]).
- Panic Safety: code wrapped in catch_unwind does not leave shared state in a broken or corrupted condition.

2. Memory, Ownership, and Lifetimes
- Unnecessary cloning: .clone() / .to_owned() where a reference (&) would work.
- Smart pointer abuse: flag excessive nesting of Arc<Mutex<Rc<RefCell<T>>>>.
- Lock contention: MutexGuard / RwLockReadGuard should drop immediately after use — scope with {} if needed.
- Lifetime inflation: simplify redundant explicit lifetimes that satisfy elision rules.
- Leak risks: reference cycles from Rc/Arc — suggest Weak to break loops.
- Allocation inside loops: initialize Vec/String/HashMap outside loops, use .clear() to reuse the allocation.

3. Error Handling and Reliability
- Ban .unwrap() in non-test code; ensure .expect() uses are safe and checked. Prefer ? propagation, match, or if let.
- Silent errors: audit `let _ = ...` that discards a Result/Option without logging or handling.
- Error context: custom errors implement std::error::Error and use thiserror/anyhow for context.
- Poisoned locks: check how mutex.lock() failures are handled — no blind .unwrap() on the lock result.

4. Performance and Data Layout
- Move Vec::new()/String::new()/HashMap::new() outside loops; use .clear() to reuse.
- Use Vec::with_capacity() / HashSet::with_capacity() when size is known.
- Struct layout: check large structs for padding waste; reorder fields largest to smallest, or #[repr(packed)] if needed.
- Prefer generics/impl Trait (static dispatch) over Box<dyn Trait>/&dyn Trait unless dynamic polymorphism is required.
- Pass large structs/arrays by reference (&T), not by value.
- Replace manual index loops (`for i in 0..vec.len()`) with iterators.
- Mark small, hot, cross-crate utility functions/trait methods #[inline].
- Watch for redundant allocations in iterator chains (e.g. .collect::<Vec<_>>().into_iter() mid-chain).
- Functions should take &str, not String, unless they intend to consume ownership (same for &[T] vs Vec<T>).
- Prefer write!/concat! over format! for hot-path string building/logging.
- Use .unwrap_or_else(|| expensive()) instead of .unwrap_or(expensive()) for lazy evaluation.
- Flag synchronous I/O or heavy CPU work inside an async fn — offload via tokio::task::spawn_blocking.
- Flag std::sync::MutexGuard held across an await point — switch to tokio::sync::Mutex if a lock must span await.
- Check atomic Ordering — flag unnecessary SeqCst where Relaxed/Acquire/Release would do.
- Flag BTreeMap/BTreeSet/linked lists in performance-critical code where Vec/VecDeque would cache better.

5. Idiomatic Design
- Prefer &str/&[T] over String/Vec<T> in function arguments.
- Replace long if/else if chains with match or if let.
- Convert manual indexing loops into iterator chains.
- Use From/Into instead of ad-hoc to_my_type() methods.
- Enforce #[deny(missing_docs)] and pub(crate) visibility to keep the public surface minimal.

6. Concurrency and Async
- Futures crossing await points must implement Send if used on a multi-threaded executor.
- Flag blocking sync calls (std::fs, std::thread::sleep) inside an async fn.
- tokio::select! branches must handle cancellation safely without leaking half-completed state.

7. Tests and Safety
- No missing unit tests for new features or bug fixes.
- No brittle/flaky tests.
- Security gaps like missing input validation or exposed secrets are addressed.
- Edge cases are covered.

Report every issue with these fields:
- Title:       one line
- Severity:    Critical | High | Medium
- Location:    file:line for every affected site
- Problem:     what is wrong and what it causes
- Fix:         what to change

If you don't have any issues to report, report back that all is fine.
```

## Step 3 — Verify and loop

Reviewer reports are claims, not facts. For every finding, read the cited code yourself and
mark it:

- **CONFIRMED** — the code does what the finding says.
- **DROPPED** — it does not, the path is unreachable, or something the reviewer missed
  already handles it. Tell the user which findings you dropped and why.

Send the CONFIRMED findings to `coder` via `SendMessage` to fix and commit. When it reports
back, read the new diff and check each finding is actually fixed — if one isn't, send it
back before the next review rather than trusting the report. Then run Step 2 again.

The loop is **clean** when a fresh reviewer's findings all come out DROPPED (or it reports
all fine), the diff fully implements the TASK, and the coder's last run of the test, lint,
and format commands passed.

## Step 4 — Security review (conditional)

Only run this step if `$RUN_SECURITY_REVIEW` is yes. If no, skip to Step 6.

Run the `security-analysis` skill against the changes the coder/reviewer loop just produced.
Do not restate its sub-agent prompts here — that copy drifted out of sync with the skill
once already. The skill owns the lanes, the checklists, the verification pass, and the
report format; this step only supplies its inputs, so it has no need to ask the user again:

- **Scope**: diff scope, base ref `$BASE`, covering the work from Steps 1–3.
- **Model**: `$SECURITY_MODEL`.

`security-analysis` is review-only — it reports and never edits. Fixing its findings is
Step 5's job, so it must not be asked to make changes, and it must not invoke this skill
back. Take its aggregated report and carry it forward.

## Step 5 — Security fix loop

If Step 4 reported all fine, skip to Step 6.

Otherwise send every CONFIRMED and PLAUSIBLE finding from the security report to `coder` via
`SendMessage` to fix and commit, then run Steps 2–3 until clean. Then run Step 4 again on
the full `$BASE..HEAD` diff. Repeat until `security-analysis` reports all fine, or reports
only findings the user has told you to accept.

## Step 6 — Final report

Tell the user:

- The branch, `$BASE`, and the commits made.
- What was implemented, per file.
- How many coder/reviewer cycles ran, and how many security rounds.
- The models used for coder, reviewer, and security review.
- Anything left open: dropped findings worth a second look, accepted security findings,
  and any test, lint, or format command that could not be made to pass.
