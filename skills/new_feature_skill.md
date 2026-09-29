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

## Step 0 — Ask before doing anything else

Before spawning any subagent, ask the user these questions and wait for their answers. Do not proceed until you have all four.

1. Which model should the `coder` subagent use? (default: `opus` if they have no preference)
2. Which model should the `reviewer` subagent use? (default: `opus`)
3. Should a security review be run after the coder/reviewer cycle is clean? (yes/no)
4. If yes to (3): which model should the `security-reviewer` subagent (and the sub-agents it spawns) use? (default: `opus`)

Refer to the chosen models below as `$CODER_MODEL`, `$REVIEWER_MODEL`, `$RUN_SECURITY_REVIEW`, and `$SECURITY_MODEL`.

Use the `cocoindex-code:ccc` skill to help with code searches throughout this workflow.

## Step 1 — Coder

Spawn `coder` using `$CODER_MODEL` as its model, with these instructions:

```
**INITIAL CODEBASE ANALYSIS:**
Use the cocoindex-code:ccc skill to find files related to the context (now and/or when you're busy with the work below), make sure to index before you start.

** Task
Implement the TASK at hand:
$ARGUMENTS

** afterwards
Make sure the following commands pass:
* cargo ci-test
* cargo ci-clippy
* cargo +nightly fmt --all

If those commands are not available in this project (not a Rust project, or the
`ci-test` / `ci-clippy` cargo aliases are not defined), fall back to running the
project's own test, lint, and format commands instead.

Then make a commit.
```

## Step 2 — Reviewer

After `coder` finishes, spawn `reviewer` using `$REVIEWER_MODEL` as its model, with these instructions:

```
Do a code review of the code.
Report back only medium and higher issues — ignore all low and coding nits, only focus on actual issues.
Also look at the following list and ensure they are fine:

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

If you don't have any issues to report, report back that all is fine.
```

## Step 3 — Coder/reviewer loop

Take the `reviewer` feedback and pass it back to `coder` (same `$CODER_MODEL`) to fix. Repeat the coder → reviewer cycle until `reviewer` reports no new issues. After each cycle, verify the fix yourself against the original issue — if it isn't actually fixed, send it back to `coder` again rather than trusting the report. Report on each cycle as it completes.

## Step 4 — Security review (conditional)

Only run this step if `$RUN_SECURITY_REVIEW` is yes. If no, skip to Step 5.

Run the `security-analysis` skill against the changes the coder/reviewer loop just produced.
Do not restate its sub-agent prompts here — that copy drifted out of sync with the skill
once already. The skill owns the lanes, the checklists, the verification pass, and the
report format; this step only supplies its inputs:

- **Scope**: diff scope, based on the merge-base of the current branch against the default
  branch, covering the work from Steps 1–3.
- **Model**: `$SECURITY_MODEL`.

`security-analysis` is review-only — it reports and never edits. Fixing its findings is
Step 5's job, so it must not be asked to make changes, and it must not invoke this skill
back. Take its aggregated report and carry it forward.

## Step 5 — Final fix loop

Give all issues found (from security review, if run) back to `coder` (`$CODER_MODEL`) to fix, then to `reviewer` (`$REVIEWER_MODEL`) to review. Repeat the coder ↔ reviewer ↔ security-reviewer cycle until `reviewer` and `security-reviewer` reports all is fine.

Report on each cycle and step throughout the whole process, including which models were used for each subagent.
