---
name: reviewer
description: Orchestrates a reviewer subagent, and optionally the security-analysis skill, over a GitHub PR, a local branch, or a set of paths, then verifies their findings into one report. Review-only. Only invoke this explicitly when the user runs /pr-reviewer or asks by name — do not auto-trigger on generic "review this" requests, since this is a heavyweight, deliberate workflow the user chooses to run.
argument-hint: [PR number/URL, branch, or paths — optionally followed by what the change is meant to do]
---

# Reviewer Orchestrator

You are the orchestrator and verifier for a review of the following **TARGET**:

```
$ARGUMENTS
```

Report to the user as each step completes, naming the model each subagent used.

This workflow is **review-only**: you and every subagent leave the user's working tree and
git history exactly as you found them. Fixing is a separate step the user invokes afterwards.

`$SKILL_DIR` below is this skill's base directory, shown when the skill loads.

## Contents

- [Subagents](#subagents)
- [Step 0 — Settle choices, scope, and index before spawning anything](#step-0--settle-choices-scope-and-index-before-spawning-anything)
  - [0a — Ask the user](#0a--ask-the-user)
  - [0b — Resolve the scope](#0b--resolve-the-scope)
  - [0c — Preflight requirements](#0c--preflight-requirements)
  - [0d — Index once](#0d--index-once)
- [Step 1 — Reviewer](#step-1--reviewer)
- [Step 2 — Verify](#step-2--verify)
- [Step 4 — Final report](#step-4--final-report)

## Subagents

`reviewer` is a label for your own tracking and reports — it is not a registered agent type.
Spawn it as a `general-purpose` agent with the model the user chose.

## Step 0 — Settle choices, scope, and index before spawning anything

### 0a — Ask the user

Ask these in a single `AskUserQuestion` call and wait for the answers:

1. **Reviewer model** — `opus` (Recommended), `sonnet`, `haiku`.

Refer to the answers below as `$REVIEWER_MODEL`

### 0b — Resolve the scope

Classify the TARGET into exactly one of:

- **PR scope** — a PR number, `#123`, or a GitHub PR URL. Run
  `gh pr view <pr> --json number,title,body,url,baseRefName,headRefOid`. Fetch the head with
  `git fetch origin pull/<number>/head` and `git fetch origin <baseRefName>`, then check it
  out into a detached worktree outside the repo:
  `git worktree add --detach "$TMPDIR/pr-reviewer-<number>" <headRefOid>`. Set `$WORKDIR` to
  that path and `$BASE` = `git -C $WORKDIR merge-base HEAD origin/<baseRefName>`. Never
  `gh pr checkout` in the user's own working tree.
- **Branch scope** — a branch name, "my changes", "this branch", or empty arguments. `$WORKDIR`
  is the current repo root and `$BASE` = `git merge-base <branch> <default-branch>`. If the
  named branch is not checked out, add a detached worktree for it as above. If there is
  uncommitted work, ask the user whether to include it (review `git diff $BASE` instead of
  `$BASE..HEAD`) or review commits only.
- **Path scope** — one or more files, directories, crates, or packages. Resolve each to a
  path on disk and confirm it exists. `$WORKDIR` is the repo root; `$BASE` is `n/a`. The
  reviewer reviews the code as it stands, not a diff.
- **Ambiguous** — matches none of these cleanly. Ask the user which they meant.

Record `$SCOPE` as one written-out line the reviewer gets verbatim: for PR and branch scope,
`git diff $BASE..HEAD` in `$WORKDIR` plus the `--stat` file list; for path scope, the list of
paths.

Set `$SPEC` — what the change is meant to do — from the first of these that exists: text the
user wrote after the target in `$ARGUMENTS`, the PR title and body, or the branch's commit
messages (`git log $BASE..HEAD`). For path scope with no user text, `$SPEC` is `none`.

If the scope resolves to zero changed files or no existing paths, stop and tell the user — a
review of nothing reads exactly like a clean review.

### 0c — Preflight requirements

Check every requirement below that applies. A skill is present when it appears in your
available-skills list; a tool is present when the check succeeds.

| Requirement | Applies when | Check | Install | If the user continues without it |
|---|---|---|---|---|
| `cocoindex-code:ccc` skill and `ccc` CLI | always | skills list; `command -v ccc` | user runs `/plugin install cocoindex-code@cocoindex-code` | the reviewer navigates with grep and direct reads |
| `gh` CLI, authenticated | PR scope | `gh auth status` | `brew install gh`, then the user runs `gh auth login` | stop — PR scope cannot be resolved |


Ask about everything missing in one `AskUserQuestion` call, as that requirements file
describes: **Install now (Recommended)**, **Continue without it**, or **Stop**. For a tool,
run the install command yourself and re-check; for a skill or a login, give the user the
step, wait for them to confirm, and re-check. Record what the user continued without as
`$UNAVAILABLE` (or `none`).

### 0d — Index once

Index `$WORKDIR` yourself with the `cocoindex-code:ccc` skill and wait for it to finish. If
indexing fails, say so and continue — the reviewer falls back to grep and direct file reads.

## Step 1b — Reviewer_2

Spawn `reviewer` on `$REVIEWER_MODEL` with these instructions, filling in `$WORKDIR`,
`$SCOPE`, `$SPEC`, and `$SKILL_DIR`:

```
Do a code review, working in $WORKDIR, of:
$SCOPE

What the change is meant to do: $SPEC

Search the existing cocoindex-code:ccc index to follow callers and related code outside the
scope. This is a review only: leave the working tree and git history exactly as you found them.

Look for the following:

* Bug: wrong result, crash, missed edge case (empty, zero, last item, rounding, time zones), a caller broken by the change, a fix applied in one caller while the shared function stays broken.
* Risk: security holes (injection, weak randomness, secrets, missing checks on input from users), data loss (errors swallowed, writes in the wrong order, no transaction).
* Scale: fine for one user, wrong for many: check-then-write races, the same work done by every process, memory or lists that only grow, a query per item, O(n^2) on big input, per-process state that must be shared.
* Missing test: risky new logic (a branch, a parser, money, security, data writes, a bug fix) with no test that fails when it breaks. One good test, not coverage.
* Speed: big slowdowns are problems. Small wins (work repeated in a hot loop) are suggestions; some software counts every millisecond.
* Lean: code that should not exist or should be smaller.
* delete: dead code, unused options, speculative features
* reuse: the repo already has this helper (name the path)
* stdlib / native: the standard library or platform already does it; a new dependency for a few lines
* yagni: abstraction with one implementation, config nobody sets
* merge: near-copies that must change together
* split: one function doing several unrelated jobs, so it is hard to read or test. Split by job, never by line count, and never into helpers that exist only to make a function shorter.

Report only Medium and higher issues — actual defects, not coding nits. If the intent above is
not "none", also check that the code fully implements it; anything missing is an issue.

For Rust code, read and apply every rule in $SKILL_DIR/../new-feat/review-checklist-rust.md
if it exists. For other languages, apply the equivalent concern where one exists.

Report every issue with these fields:
- Title:       one line
- Severity:    Critical | High | Medium
- Location:    file:line for every affected site
- Problem:     In very simple English, what is wrong and what it causes. Use short sentences, everyday words. Explain a technical term the first time you use it. The reader may never have seen this code.
- Fix:         what to change

Where the intent can be read two ways and the code commits to one, report a Question
instead of an issue:
- Title:       one line
- Location:    file:line where the code commits to a reading
- Readings:    each plausible reading, and which one the code took

If you have no issues or Questions to report, report back that all is fine.
```

## Step 1a — Reviewer_1

Spawn `reviewer` on `$REVIEWER_MODEL` with these instructions, filling in `$WORKDIR`,
`$SCOPE`, `$SPEC`, and `$SKILL_DIR`:

```
Do a code review, working in $WORKDIR, of:
$SCOPE using the skill code-review

What the change is meant to do: $SPEC

Search the existing cocoindex-code:ccc index to follow callers and related code outside the
scope. This is a review only: leave the working tree and git history exactly as you found them.

Report only Medium and higher issues — actual defects, not coding nits. If the intent above is
not "none", also check that the code fully implements it; anything missing is an issue.

For Rust code, read and apply every rule in $SKILL_DIR/../new-feat/review-checklist-rust.md
if it exists. For other languages, apply the equivalent concern where one exists.

Report every issue with these fields:
- Title:       one line
- Severity:    Critical | High | Medium
- Location:    file:line for every affected site
- Problem:     In very simple English, what is wrong and what it causes. Use short sentences, everyday words. Explain a technical term the first time you use it. The reader may never have seen this code.
- Fix:         what to change

Where the intent can be read two ways and the code commits to one, report a Question
instead of an issue:
- Title:       one line
- Location:    file:line where the code commits to a reading
- Readings:    each plausible reading, and which one the code took

If you have no issues or Questions to report, report back that all is fine.
```

## Step 1c — Reviewer_3_RUST

Only spawn this reviewer if the code changed is rust only. This model should only look at rut code
Spawn `reviewer` on `$REVIEWER_MODEL` with these instructions, filling in `$WORKDIR`,
`$SCOPE`, `$SPEC`, and `$SKILL_DIR`:

```
Do a code review, working in $WORKDIR, of:
$SCOPE

What the change is meant to do: $SPEC

Search the existing cocoindex-code:ccc index to follow callers and related code outside the
scope. This is a review only: leave the working tree and git history exactly as you found them.

Look for the following:
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

Report only Medium and higher issues — actual defects, not coding nits. If the intent above is
not "none", also check that the code fully implements it; anything missing is an issue.

For Rust code, read and apply every rule in $SKILL_DIR/../new-feat/review-checklist-rust.md
if it exists. For other languages, apply the equivalent concern where one exists.

Report every issue with these fields:
- Title:       one line
- Severity:    Critical | High | Medium
- Location:    file:line for every affected site
- Problem:     In very simple English, what is wrong and what it causes. Use short sentences, everyday words. Explain a technical term the first time you use it. The reader may never have seen this code.
- Fix:         what to change

Where the intent can be read two ways and the code commits to one, report a Question
instead of an issue:
- Title:       one line
- Location:    file:line where the code commits to a reading
- Readings:    each plausible reading, and which one the code took

If you have no issues or Questions to report, report back that all is fine.
```

## Step 2 — Verify

Aggregate reports from Reviewers, their reports are claims, not facts. For every finding, read the cited code yourself and
mark it:

- **CONFIRMED** — the code does what the finding says.
- **DROPPED** — it does not, the path is unreachable, or something the reviewer missed
  already handles it. Tell the user which findings you dropped and why.

Pass reviewer Questions through to the final report as open questions for the author — do
not resolve them by guessing.



## Step 4 — Final report

Tell the user:

- The scope: PR number and URL, or branch, `$BASE`, and the files reviewed; or the paths.
- `$SPEC`, and whether the code fully implements it.
- Every CONFIRMED code-review finding, most severe first, with its fields.
- Every CONFIRMED and PLAUSIBLE security finding, from the `security-analysis` report.
- Open questions for the author.
- Dropped findings worth a second look, and everything in `$UNAVAILABLE`.
- The models used for the reviewer and the security review.

If you created a worktree in Step 0b, remove it with `git worktree remove --force <path>`
once the report is written.

List all findings in the following format:
- Title:       one line
- Severity:    Critical | High | Medium
- Location:    file:line for every affected site
- Problem:     In very simple English, what is wrong and what it causes. Use short sentences, everyday words. Explain a technical term the first time you use it. The reader may never have seen this code.
- Fix:         what to change
