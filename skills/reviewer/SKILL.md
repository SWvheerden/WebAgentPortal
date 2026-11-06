---
name: reviewer
description: Orchestrates up to three parallel reviewer subagents (the code-review skill, a general checklist, and a Rust-specific checklist) over a GitHub PR, a local branch, or a set of paths, then verifies and de-duplicates their findings into one report. Review-only. Only invoke this explicitly when the user runs /reviewer or asks by name — do not auto-trigger on generic "review this" requests, since this is a heavyweight, deliberate workflow the user chooses to run.
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
- [Called from another skill](#called-from-another-skill)
- [Step 0 — Settle choices, scope, and index before spawning anything](#step-0--settle-choices-scope-and-index-before-spawning-anything)
  - [0a — Ask the user](#0a--ask-the-user)
  - [0b — Resolve the scope](#0b--resolve-the-scope)
  - [0c — Preflight requirements](#0c--preflight-requirements)
  - [0d — Index once](#0d--index-once)
- [Step 1 — Reviewers](#step-1--reviewers)
  - [1a — code-review reviewer](#1a--code-review-reviewer)
  - [1b — Checklist reviewer](#1b--checklist-reviewer)
  - [1c — Rust reviewer (conditional)](#1c--rust-reviewer-conditional)
- [Step 2 — Verify](#step-2--verify)
- [Step 3 — Final report](#step-3--final-report)

## Subagents

`reviewer-1`, `reviewer-2`, and `reviewer-3-rust` are labels for your own tracking and
reports — they are not registered agent types. Spawn each as a fresh `general-purpose` agent
with `$REVIEWER_MODEL`, all in one message so they run in parallel, and wait for every one
to report back before Step 2.

Every reviewer reports in this **finding format**, which Step 3 also uses:

```
Issue:
- Title:       one line
- Severity:    Critical | High | Medium
- Location:    file:line for every affected site
- Problem:     In very simple English, what is wrong and what it causes. Use short sentences,
               everyday words. Explain a technical term the first time you use it. The reader
               may never have seen this code.
- Fix:         what to change

Question (where the intent can be read two ways and the code commits to one):
- Title:       one line
- Location:    file:line where the code commits to a reading
- Readings:    each plausible reading, and which one the code took
```

Paste that block into each reviewer prompt where it says `<finding format>`.

## Called from another skill

Another skill (such as `new-feat` or `pr-reviewer`) may run this one and supply some inputs.
Use each supplied input as given and skip the part of Step 0 that would set it:

- **Model** → `$REVIEWER_MODEL`; skip 0a.
- **Scope** — `$WORKDIR`, plus `$BASE` and whether to include uncommitted work, or the paths
  → skip 0b's classification and questions. Still build `$SCOPE`, set `$HAS_RUST`, and run
  the empty-scope check.
- **Spec** → `$SPEC`, used verbatim.
- **Preflight done**, with `$UNAVAILABLE` → skip 0c.
- **Index done** → skip 0d.

Then run Steps 1–3 as written. The Step 3 report goes back to the calling skill, which
decides what to do with it. Never remove a worktree the caller created.

## Step 0 — Settle choices, scope, and index before spawning anything

### 0a — Ask the user

Ask this in one `AskUserQuestion` call and wait for the answer:

1. **Reviewer model** — `opus` (Recommended), `sonnet`, `haiku`. Every reviewer uses it.

Refer to the answer below as `$REVIEWER_MODEL`.

### 0b — Resolve the scope

Classify the TARGET into exactly one of:

- **PR scope** — a PR number, `#123`, or a GitHub PR URL. Run
  `gh pr view <pr> --json number,title,body,url,baseRefName,headRefOid`. Fetch the head with
  `git fetch origin pull/<number>/head` and `git fetch origin <baseRefName>`, then check it
  out into a detached worktree outside the repo:
  `git worktree add --detach "$TMPDIR/reviewer-<number>" <headRefOid>`. Set `$WORKDIR` to
  that path and `$BASE` = `git -C $WORKDIR merge-base HEAD origin/<baseRefName>`. Never
  `gh pr checkout` in the user's own working tree.
- **Branch scope** — a branch name, "my changes", "this branch", or empty arguments. `$WORKDIR`
  is the current repo root and `$BASE` = `git merge-base <branch> <default-branch>`. If the
  named branch is not checked out, add a detached worktree for it as above. If there is
  uncommitted work, ask the user whether to include it (review `git diff $BASE` instead of
  `$BASE..HEAD`) or review commits only.
- **Path scope** — one or more files, directories, crates, or packages. Resolve each to a
  path on disk and confirm it exists. `$WORKDIR` is the repo root; `$BASE` is `n/a`. The
  reviewers review the code as it stands, not a diff.
- **Ambiguous** — matches none of these cleanly. Ask the user which they meant.

Record `$SCOPE` as one written-out line every reviewer gets verbatim: for PR and branch
scope, `git diff $BASE..HEAD` in `$WORKDIR` plus the `--stat` file list; for path scope, the
list of paths.

Set `$SPEC` — what the change is meant to do — from the first of these that exists: text the
user wrote after the target in `$ARGUMENTS`, the PR title and body, or the branch's commit
messages (`git log $BASE..HEAD`). For path scope with no user text, `$SPEC` is `none`.

Set `$HAS_RUST` to yes if any `.rs` file is in scope (or `Cargo.toml`/`Cargo.lock` changed),
otherwise no.

If the scope resolves to zero changed files or no existing paths, stop and tell the user — a
review of nothing reads exactly like a clean review.

### 0c — Preflight requirements

Check every requirement below that applies. A skill is present when it appears in your
available-skills list; a tool is present when the check succeeds.

| Requirement | Applies when | Check | Install | If the user continues without it |
|---|---|---|---|---|
| `cocoindex-code:ccc` skill and `ccc` CLI | always | skills list; `command -v ccc` | user runs `/plugin install cocoindex-code@cocoindex-code` | the reviewers navigate with grep and direct reads instead of the cocoindex index |
| `code-review` skill | always | skills list | ships with Claude Code; the user updates Claude Code | `reviewer-1` reviews without it, using its own judgement |
| `gh` CLI, authenticated | PR scope | `gh auth status` | `brew install gh`, then the user runs `gh auth login` | stop — PR scope cannot be resolved |

Ask about everything missing in one `AskUserQuestion` call with the options **Install now
(Recommended)**, **Continue without it**, or **Stop**. For a tool, run the install command
yourself and re-check; for a skill or a login, give the user the step, wait for them to
confirm, and re-check. Record what the user continued without as `$UNAVAILABLE` (or `none`).

### 0d — Index once

Index `$WORKDIR` yourself with the `cocoindex-code:ccc` skill and wait for it to finish. If
indexing fails, say so and continue — the reviewers fall back to grep and direct file reads.

## Step 1 — Reviewers

Spawn `reviewer-1` and `reviewer-2` always, and `reviewer-3-rust` only if `$HAS_RUST` is
yes — all in one message. Fill in `$WORKDIR`, `$SCOPE`, `$SPEC`, `$SKILL_DIR`, and
`<finding format>` in each prompt.

### 1a — code-review reviewer

Spawn `reviewer-1` with these instructions:

```
Do a code review, working in $WORKDIR, of:
$SCOPE

What the change is meant to do: $SPEC

Run the `code-review` skill at effort level `high` on this scope (a PR number, the branch,
or the paths). Never pass --fix or --comment. If the skill is unavailable, review the scope
yourself.

Use the cocoindex-code:ccc skill to search the existing index for files, callers, and related
code outside the scope. Fall back to grep and direct reads only if it fails. This is a review
only: leave the working tree and git history exactly as you found them.

Report only Medium and higher issues — actual defects, not coding nits. If the intent above is
not "none", also check that the code fully implements it; anything missing is an issue.

Report every issue and Question in this format, converting the skill's output into it:
<finding format>

If you have no issues or Questions to report, report back that all is fine.
```

### 1b — Checklist reviewer

Spawn `reviewer-2` with these instructions:

```
Do a code review, working in $WORKDIR, of:
$SCOPE

What the change is meant to do: $SPEC

Use the cocoindex-code:ccc skill to search the existing index for files, callers, and related
code outside the scope. Fall back to grep and direct reads only if it fails. This is a review
only: leave the working tree and git history exactly as you found them.

Look for the following:

* Bug: wrong result, crash, missed edge case (empty, zero, last item, rounding, time zones), a caller broken by the change, a fix applied in one caller while the shared function stays broken.
* Risk: security holes (injection, weak randomness, secrets, missing checks on input from users), data loss (errors swallowed, writes in the wrong order, no transaction).
* Scale: fine for one user, wrong for many: check-then-write races, the same work done by every process, memory or lists that only grow, a query per item, O(n^2) on big input, per-process state that must be shared.
* Missing test: risky new logic (a branch, a parser, money, security, data writes, a bug fix) with no test that fails when it breaks. One good test, not coverage.
* Speed: big slowdowns only. Skip small wins.
* Lean: code that should not exist or should be smaller, where it costs real maintenance:
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

Report every issue and Question in this format:
<finding format>

If you have no issues or Questions to report, report back that all is fine.
```

### 1c — Rust reviewer (conditional)

Only spawn `reviewer-3-rust` if `$HAS_RUST` is yes. It reviews only the Rust code in scope.

```
Do a code review, working in $WORKDIR, of the Rust code (.rs files, Cargo.toml) in:
$SCOPE

Ignore files in other languages.

What the change is meant to do: $SPEC

Use the cocoindex-code:ccc skill to search the existing index for files, callers, and related
code outside the scope. Fall back to grep and direct reads only if it fails. This is a review
only: leave the working tree and git history exactly as you found them.

Read and apply every rule in $SKILL_DIR/../new-feat/review-checklist-rust.md if it exists.

Look for the following:

1. Safety and soundness (highest priority)
- Every unsafe block has a // SAFETY: comment explaining why it is sound.
- Undefined behaviour: raw pointer casts, transmute, and UnsafeCell use that can cause data races or invalid values.
- FFI boundaries: memory allocated by C is freed by C and vice versa; types crossing the boundary are #[repr(C)].
- Panic safety: code wrapped in catch_unwind does not leave shared state broken.

2. Memory, ownership, and lifetimes
- Unnecessary .clone() / .to_owned() where a reference would work, on a hot path or with large data.
- Deeply nested smart pointers (e.g. Arc<Mutex<..>> wrapping more RefCell/Rc layers) that hide ownership.
- Lock guards (MutexGuard, RwLock guards) held longer than needed — scope them with {}.
- Rc/Arc reference cycles that leak — use Weak to break them.

3. Error handling and reliability
- .unwrap() in non-test code; .expect() only where the invariant is guaranteed. Prefer ?, match, or if let.
- `let _ = ...` that silently discards a Result.
- Errors without context: use the crate's existing error type (thiserror/anyhow if it already uses them).
- Poisoned locks: mutex.lock() results handled deliberately, not blindly unwrapped.

4. Performance (report only on hot paths or large inputs)
- Allocations inside loops: create Vec/String/HashMap outside and .clear() to reuse.
- Vec::with_capacity() / HashMap::with_capacity() when the size is known.
- Large structs/arrays passed by value instead of by reference.
- Functions taking String/Vec<T> where &str/&[T] would do, unless they consume ownership.
- Redundant allocations in iterator chains (e.g. .collect::<Vec<_>>().into_iter() mid-chain).
- .unwrap_or(expensive()) instead of .unwrap_or_else(|| expensive()).
- Box<dyn Trait> where generics/impl Trait would do and dynamic dispatch is not needed.
- Unneeded SeqCst atomic ordering where Acquire/Release/Relaxed is correct.

5. Concurrency and async
- Blocking work (std::fs, std::thread::sleep, heavy CPU) inside an async fn — use tokio::task::spawn_blocking or async equivalents.
- std::sync::MutexGuard held across an .await — use tokio::sync::Mutex if a lock must span an await.
- Values held across .await that are not Send when the future runs on a multi-threaded executor.
- tokio::select! branches that are not cancellation-safe and leave half-completed state.

6. Idiomatic design (report only where it hides a defect or makes the code hard to change)
- Long if/else-if chains that should be match or if let.
- Ad-hoc to_my_type() methods instead of From/Into.
- New public items that could be pub(crate).

7. Tests
- Missing unit tests for new features or bug fixes.
- Brittle or flaky tests (timing, ordering, shared global state).
- Edge cases and input validation not covered.

Report only Medium and higher issues — actual defects, not coding nits. If the intent above is
not "none", also check that the Rust code fully implements it; anything missing is an issue.

Report every issue and Question in this format:
<finding format>

If you have no issues or Questions to report, report back that all is fine.
```

## Step 2 — Verify

Wait for every reviewer you spawned. Their reports are claims, not facts.

First merge duplicates: findings from different reviewers that point at the same root cause
become one finding, noting which reviewers raised it. Then, for every finding, read the cited
code yourself and mark it:

- **CONFIRMED** — the code does what the finding says.
- **DROPPED** — it does not, the path is unreachable, or something the reviewer missed
  already handles it. Tell the user which findings you dropped and why.

Pass reviewer Questions through to the final report as open questions for the author — do
not resolve them by guessing.

## Step 3 — Final report

Tell the user:

- The scope: PR number and URL, or branch, `$BASE`, and the files reviewed; or the paths.
- `$SPEC`, and whether the code fully implements it.
- Every CONFIRMED finding, most severe first, in the finding format, plus which reviewers
  raised it.
- Open questions for the author.
- Dropped findings worth a second look, and everything in `$UNAVAILABLE`.
- Which reviewers ran, and `$REVIEWER_MODEL`.

If you created a worktree yourself in Step 0b, remove it with `git worktree remove --force <path>`
once the report is written.
