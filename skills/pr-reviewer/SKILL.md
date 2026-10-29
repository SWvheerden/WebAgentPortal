---
name: pr-reviewer
description: Orchestrates a reviewer subagent, and optionally the security-analysis skill, over a GitHub PR, a local branch, or a set of paths, then verifies their findings into one report. Review-only. Only invoke this explicitly when the user runs /pr-reviewer or asks by name — do not auto-trigger on generic "review this" requests, since this is a heavyweight, deliberate workflow the user chooses to run.
argument-hint: [PR number/URL, branch, or paths — optionally followed by what the change is meant to do]
---

# PR Reviewer Orchestrator

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
- [Step 3 — Security review (conditional)](#step-3--security-review-conditional)
- [Step 4 — Final report](#step-4--final-report)
- [Step 5 — Post to the PR (PR scope only)](#step-5--post-to-the-pr-pr-scope-only)

## Subagents

`reviewer` is a label for your own tracking and reports — it is not a registered agent type.
Spawn it as a `general-purpose` agent with the model the user chose.

## Step 0 — Settle choices, scope, and index before spawning anything

### 0a — Ask the user

Ask these in a single `AskUserQuestion` call and wait for the answers:

1. **Reviewer model** — `opus` (Recommended), `sonnet`, `haiku`.
2. **Security review** after the code review — `Yes, opus` (Recommended), `Yes, sonnet`,
   `No`. The model chosen here is used by `security-analysis` and every sub-agent it spawns.

Refer to the answers below as `$REVIEWER_MODEL`, `$RUN_SECURITY_REVIEW`, and
`$SECURITY_MODEL`.

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
| `new-feat` skill (for its Rust checklist) | the scope contains Rust | `$SKILL_DIR/../new-feat/review-checklist-rust.md` exists | user installs it into `~/.claude/skills/new-feat/` | the reviewer applies general Rust review judgement |
| `security-analysis` skill | `$RUN_SECURITY_REVIEW` is yes | skills list | user installs it into `~/.claude/skills/security-analysis/` | `$RUN_SECURITY_REVIEW` becomes no |

If `$RUN_SECURITY_REVIEW` is yes, also run the checks in
`$SKILL_DIR/../security-analysis/requirements.md` now, so every question comes before any
work starts.

Ask about everything missing in one `AskUserQuestion` call, as that requirements file
describes: **Install now (Recommended)**, **Continue without it**, or **Stop**. For a tool,
run the install command yourself and re-check; for a skill or a login, give the user the
step, wait for them to confirm, and re-check. Record what the user continued without as
`$UNAVAILABLE` (or `none`).

### 0d — Index once

Index `$WORKDIR` yourself with the `cocoindex-code:ccc` skill and wait for it to finish. If
indexing fails, say so and continue — the reviewer falls back to grep and direct file reads.

## Step 1 — Reviewer

Spawn `reviewer` on `$REVIEWER_MODEL` with these instructions, filling in `$WORKDIR`,
`$SCOPE`, `$SPEC`, and `$SKILL_DIR`:

```
Do a code review, working in $WORKDIR, of:
$SCOPE

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
- Problem:     what is wrong and what it causes
- Fix:         what to change

Where the intent can be read two ways and the code commits to one, report a Question
instead of an issue:
- Title:       one line
- Location:    file:line where the code commits to a reading
- Readings:    each plausible reading, and which one the code took

If you have no issues or Questions to report, report back that all is fine.
```

## Step 2 — Verify

Reviewer reports are claims, not facts. For every finding, read the cited code yourself and
mark it:

- **CONFIRMED** — the code does what the finding says.
- **DROPPED** — it does not, the path is unreachable, or something the reviewer missed
  already handles it. Tell the user which findings you dropped and why.

Pass reviewer Questions through to the final report as open questions for the author — do
not resolve them by guessing.

## Step 3 — Security review (conditional)

Only run this step if `$RUN_SECURITY_REVIEW` is yes. If no, skip to Step 4.

Run the `security-analysis` skill from `$WORKDIR`. That skill owns the lanes, checklists,
verification, and report format; supply its inputs so it needs nothing more from the user:

- **Scope**: diff scope with base ref `$BASE` for PR and branch scope; path scope with the
  resolved paths otherwise.
- **Model**: `$SECURITY_MODEL`.
- **Preflight**: done, with these unavailable: `$UNAVAILABLE`.

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

## Step 5 — Post to the PR (PR scope only)

Ask the user with `AskUserQuestion` whether to post the report to the PR:
**Don't post (Recommended)** or **Post as a PR review comment**. Only if they choose to post,
write the report to a temp file and run `gh pr review <number> --comment --body-file <file>`,
then give the user the comment's URL.
