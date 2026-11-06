---
name: new-feat
description: Orchestrates a coder subagent, the reviewer skill, and (optionally) the security-analysis skill to implement a fix or feature end-to-end, looping review cycles until clean. Only invoke this explicitly when the user runs /new-feat or asks by name — do not auto-trigger on generic "fix this" or "implement X" requests, since this is a heavyweight, deliberate workflow the user chooses to run.
argument-hint: [task description]
---

# New Feature Orchestrator

You are the orchestrator and verifier for the following **TASK**:

```
$ARGUMENTS
```

Report to the user as each step and each cycle completes, naming the model each subagent used.

`$SKILL_DIR` below is this skill's base directory, shown when the skill loads.

## Contents

- [Subagents](#subagents)
  - [Open questions and assumptions](#open-questions-and-assumptions)
- [Step 0 — Settle choices, branch, and index before spawning anything](#step-0--settle-choices-branch-and-index-before-spawning-anything)
  - [0a — Ask the user](#0a--ask-the-user)
  - [0b — Branch and base ref](#0b--branch-and-base-ref)
  - [0c — Preflight requirements](#0c--preflight-requirements)
  - [0d — Index once](#0d--index-once)
- [Step 1 — Coder](#step-1--coder)
- [Step 2 — Review](#step-2--review)
- [Step 3 — Fix and loop](#step-3--fix-and-loop)
- [Step 4 — Security review (conditional)](#step-4--security-review-conditional)
- [Step 5 — Security fix loop](#step-5--security-fix-loop)
- [Step 6 — Final report](#step-6--final-report)

## Subagents

`coder` is a label for your own tracking and reports — it is not a registered agent type.
Spawn it as a `general-purpose` agent with the model the user chose.

- **coder** — spawn it once, in Step 1, and keep its ID. Every later fix round goes to that
  same agent via `SendMessage`, so it keeps its context of the change it is making.
- **reviews** run through the `reviewer` skill, which spawns fresh reviewer subagents every
  round, so each round reads the code cold.

**Free disk while waiting**: in a Rust repo checked out as a git worktree (`git rev-parse
--git-dir` differs from `git rev-parse --git-common-dir`), each time `coder` has reported
back and you are waiting on a `reviewer` run or `security-analysis`, delete the worktree's `target/`
folder (`rm -rf <worktree-root>/target`). Only delete a `target/` inside this worktree —
never one in the main checkout or a shared `CARGO_TARGET_DIR`. Skip it while `coder` is still
working.

**Cycle cap**: if a loop in Step 3 or Step 5 runs 5 cycles without coming back clean, stop
and ask the user how to proceed, listing the findings still open.

### Open questions and assumptions

Subagents cannot ask the user; they report back to you. Keep a `$DECISIONS` list (initially
`none`) of every clarification the user gives. Pass it, with the TASK, to every coder
message and every `reviewer` run, so later rounds build on the answers.

When a report contains an **open question** — from the coder, or a reviewer's Question item:

1. Answer it yourself if the codebase or the TASK settles it, and add the answer to
   `$DECISIONS`.
2. Otherwise ask the user with `AskUserQuestion`, offering the reported options with the
   recommendation first, and add the answer to `$DECISIONS`.
3. Send the answer to `coder` via `SendMessage` so it continues with its context intact.

Treat each **assumption** in a coder report like a reviewer finding: read the code it
touches. If the codebase supports it, keep it; if it looks wrong, ask the user as above.
Keep a list of assumptions the user never confirmed for Step 6.

## Step 0 — Settle choices, branch, and index before spawning anything

### 0a — Ask the user

Ask these in a single `AskUserQuestion` call and wait for the answers:

1. **Coder model** — `opus` (Recommended), `sonnet`, `haiku`.
2. **Reviewer model** — `opus` (Recommended), `sonnet`, `haiku`. Every reviewer subagent the
   `reviewer` skill spawns uses it.
3. **Security review** after the coder/reviewer loop is clean — `Yes, opus` (Recommended),
   `Yes, sonnet`, `No`. The model chosen here is used by `security-analysis` and every
   sub-agent it spawns.

Refer to the answers below as `$CODER_MODEL`, `$REVIEWER_MODEL`, `$RUN_SECURITY_REVIEW`,
and `$SECURITY_MODEL`.

### 0b — Branch and base ref

- If the working tree has uncommitted changes, ask the user whether to include them, stash
  them, or stop. Coder commits contain only this TASK's work.
- If the current branch is the default branch, create a feature branch named after the TASK
  (e.g. `feat/<short-slug>` or `fix/<short-slug>`) and switch to it.
- Record `$BASE` = `git merge-base HEAD <default-branch>`. Every review below is scoped to
  `git diff $BASE..HEAD`.

### 0c — Preflight requirements

Check every requirement below that applies. A skill is present when it appears in your
available-skills list; a tool is present when the check succeeds.

| Requirement | Applies when | Check | Install | If the user continues without it |
|---|---|---|---|---|
| `cocoindex-code:ccc` skill and `ccc` CLI | always | skills list; `command -v ccc` | user runs `/plugin install cocoindex-code@cocoindex-code` | subagents navigate with grep and direct reads |
| `reviewer` skill | always | skills list | user installs it into `~/.claude/skills/reviewer/` | stop — Step 2 needs it |
| `security-analysis` skill | `$RUN_SECURITY_REVIEW` is yes | skills list | user installs it into `~/.claude/skills/security-analysis/` | `$RUN_SECURITY_REVIEW` becomes no |
| `cargo-nextest` | `.cargo/config.toml` defines the `ci-*` aliases | `command -v cargo-nextest` | `cargo install cargo-nextest --locked` | the coder skips `cargo ci-test` |
| `cargo-lints` | same | `command -v cargo-lints` | `cargo install cargo-lints` | the coder skips `cargo ci-clippy` |
| `cargo-machete` | same | `command -v cargo-machete` | `cargo install cargo-machete --locked` | the coder skips `cargo machete` |
| nightly `rustfmt` | same | `rustup run nightly rustfmt --version` | `rustup toolchain install nightly --component rustfmt` | the coder skips `cargo +nightly ci-fmt-fix` |
| `rg` (ripgrep) | `scripts/file_license_check.sh` exists | `command -v rg` | `brew install ripgrep` | the coder skips the licence check |

Also run the checks in the `reviewer` skill's 0c table
(`$SKILL_DIR/../reviewer/SKILL.md`) that apply to branch scope, and, if
`$RUN_SECURITY_REVIEW` is yes, the checks in `$SKILL_DIR/../security-analysis/requirements.md`,
so every question comes before any work starts.

Ask about everything missing in one `AskUserQuestion` call with the options **Install now
(Recommended)**, **Continue without it**, or **Stop**. For a tool, run the install command
yourself and re-check; for a skill, give the user the install step, wait for them to confirm,
and re-check. Record what the user continued without as
`$UNAVAILABLE` (or `none`) — the coder skips those commands, and Step 6 lists them.

The preflight is **done** when every requirement is present or the user has chosen to
continue without it.

### 0d — Index once

Index the codebase yourself with the `cocoindex-code:ccc` skill and wait for it to finish.
Every subagent below searches that index. If indexing fails, say so and continue —
subagents fall back to grep and direct file reads.

## Step 1 — Coder

Spawn `coder` on `$CODER_MODEL` with these instructions, filling in `$UNAVAILABLE` and
`$DECISIONS`:

```
** Codebase search
Search the existing cocoindex-code:ccc index to find files, callers, and related code.

** Style
Skip features, options and flexibility nobody asked for, and list each one under "Skipped" in your report.
Already in this codebase (a helper, component, service, pattern)? Use it the way the surrounding code does.
Standard library or a platform feature? Use it, unless the project has its own. A house component beats a native widget.
An installed dependency? Use it. Never add a dependency for a few lines.
Can it be one line a reader gets at a glance? One line. A one-liner that needs decoding is not short.
Otherwise: the minimum code that works.

Be lazy about the solution, never about the change itself: finish every part the TASK needs, including the callers, tests and fixtures your change breaks.
The shortest working diff wins, once you know everything it must touch.
No abstraction, wrapper, type conversion, option, config, boilerplate, error handling for cases that don't exist yet, or "for later" code. Keep values in the form the platform already gives you. Deletion beats addition.
Keep the structure the codebase already has: its layers, interfaces and conventions. Don't refactor, rename, or tidy code the TASK doesn't touch.
Bug fix: before you edit, find every caller of the function you touch, then fix the root cause once in the shared code.
Code you move or merge keeps its error handling and validation.
Between options of equal size, take the one that is correct on edge cases.
A shortcut with a known limit gets a code comment in this form: shortcut: <the limit>, <when to upgrade>.
Never cut: validation at trust boundaries, error handling that prevents data loss, security, accessibility, anything the user asked for.

Comments: only the why the code cannot show, in a line or two. No code history: don't describe what changed, what used to be there, or which review round prompted it — that belongs in the commit message.

** Task
Implement the TASK at hand:
$ARGUMENTS

Decisions the user has already made: $DECISIONS

Add or update tests covering the new behaviour or the fixed bug. New non-trivial logic (a branch, a loop, a parser, money or security code) gets at least one small test; trivial changes need none.

** When the TASK is unclear
If a choice changes behaviour, a public interface, data formats, or scope — or two readings
of the TASK lead to different code — stop before implementing it and report back:
  Question:  one line
  Options:   the realistic choices, with your recommendation first
  Impact:    what each choice changes
For smaller choices, pick the option that matches the existing code, keep going, and list
it under "Assumptions" in your report.

** Afterwards
Run these from the repo root (the Tari CI checks); every one must pass:
* cargo +nightly ci-fmt-fix
* cargo ci-clippy
* cargo machete
* cargo ci-check
* cargo ci-test
* ./scripts/file_license_check.sh

In a repo without these cargo aliases, run its own format, lint, and test commands instead.
Skip these, which are unavailable for this run, and list them as not run: $UNAVAILABLE

Then make a commit on the current branch.

** Report back
- What you changed and why, per file.
- The commit hash.
- Each command you ran and whether it passed. If one failed and you could not fix it,
  say so and paste the relevant output.
- Assumptions: each smaller choice you made, with file:line, or "none".
- Skipped: each feature, option or flexibility you chose not to add, one line each, or "none".
```

## Step 2 — Review

Run the `reviewer` skill. Supply its inputs so it needs nothing from the user:

- **Model**: `$REVIEWER_MODEL`.
- **Scope**: `$WORKDIR` is the repo root, base `$BASE`, commits only (`git diff $BASE..HEAD`).
- **Spec**: the TASK, followed by "Decisions the user has already made, which are part of the
  TASK: $DECISIONS".
- **Preflight**: done, with these unavailable: `$UNAVAILABLE`.
- **Index**: done.

It runs its reviewers in parallel, verifies and de-duplicates their findings, and returns
CONFIRMED findings, DROPPED findings, and open Questions. Every run spawns fresh reviewers.

## Step 3 — Fix and loop

The `reviewer` skill has already verified each finding by reading the code, so work from its
CONFIRMED list. Tell the user which findings it dropped and why.

Route its Questions through [Open questions and assumptions](#open-questions-and-assumptions);
when the answer differs from the diff's reading, send it to `coder` as a finding.

Send the CONFIRMED findings to `coder` via `SendMessage` to fix and commit. When it reports
back, read the new diff and check each finding is actually fixed — send any that aren't
back before the next review. Then run Step 2 again.

The loop is **clean** when a `reviewer` run returns no CONFIRMED findings and no open
Questions, every open question is answered, the diff fully implements the TASK, and the
coder's last run of the Step 1 commands passed.

## Step 4 — Security review (conditional)

Only run this step if `$RUN_SECURITY_REVIEW` is yes. If no, skip to Step 6.

Run the `security-analysis` skill against the changes from Steps 1–3. That skill owns the
lanes, checklists, verification, and report format; supply its inputs so it needs nothing
more from the user:

- **Scope**: diff scope, base ref `$BASE`.
- **Model**: `$SECURITY_MODEL`.
- **Preflight**: done, with these unavailable: `$UNAVAILABLE`.

`security-analysis` is review-only: its aggregated report comes back here, and Step 5 does
the fixing.

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
- `$DECISIONS`: each clarification the user gave.
- Anything left open: dropped findings worth a second look, accepted security findings,
  any Step 1 command that could not be made to pass, everything in `$UNAVAILABLE`, and
  coder assumptions the user never confirmed.
