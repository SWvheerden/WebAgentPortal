---
name: security-analysis
description: Orchestrates parallel security-review subagents over a piece of code, a package, or a set of changes, then chains, verifies, and de-duplicates their findings into one report. Only invoke this explicitly when the user runs /security-analysis or asks by name — do not auto-trigger on generic "scan this" requests, since this is a heavyweight, deliberate workflow the user chooses to run.
argument-hint: [scope — paths/packages to review, or a branch/PR/diff]
---

# Orchestrator

You are the orchestrator for a security analysis of the following **CODE**:

```
$ARGUMENTS
```

You are in charge of the security review. Report back every issue found, at every severity.
If you don't have any issues to report, report back that all is fine.
Ignore coding nits — style, naming, formatting, and preference-level refactors. Everything
else gets reported and marked with a severity, including Low.

This workflow is **review-only**: you and every sub-agent leave the working tree and git
history exactly as you found them. Fixing is a separate step the user invokes afterwards.

`$SKILL_DIR` below is this skill's base directory, shown when the skill loads. Each lane's
brief lives in `$SKILL_DIR/lanes/`.

## Contents

- [Step 0 — Resolve scope, model, and project profile before spawning anything](#step-0--resolve-scope-model-and-project-profile-before-spawning-anything)
  - [0a — Resolve the review scope](#0a--resolve-the-review-scope)
  - [0b — Resolve the model](#0b--resolve-the-model)
  - [0c — Detect the project profile](#0c--detect-the-project-profile)
  - [0d — Sanity-check before spawning](#0d--sanity-check-before-spawning)
  - [0e — Preflight requirements](#0e--preflight-requirements)
  - [0f — Index the codebase](#0f--index-the-codebase)
  - [0g — Build the shared preamble](#0g--build-the-shared-preamble)
- [Step 1 — Wave 1: spawn the scan lanes in parallel](#step-1--wave-1-spawn-the-scan-lanes-in-parallel)
- [Step 2 — Wave 2: attack-path chaining](#step-2--wave-2-attack-path-chaining)
- [Step 3 — Verify before reporting](#step-3--verify-before-reporting)
- [Step 4 — Aggregate and report](#step-4--aggregate-and-report)

## Step 0 — Resolve scope, model, and project profile before spawning anything

### 0a — Resolve the review scope

Turn `$ARGUMENTS` into one concrete, written-out scope. Every sub-agent is handed this scope
verbatim, so a vague scope like "the new code" is how this workflow silently reviews the
wrong thing — or nothing at all.

Classify `$ARGUMENTS` into exactly one of:

- **Diff scope** — the arguments mention a branch, a diff, a PR, "my changes", or "the new
  code", or the arguments are empty. Resolve it by finding the base ref and the changed
  files: `git merge-base HEAD <default-branch>`, then `git diff --stat <base>..HEAD`, plus
  `git status --porcelain` for uncommitted work.
- **Path scope** — the arguments name one or more files, directories, crates, or packages.
  Resolve each to an actual path on disk and confirm it exists.
- **Ambiguous** — the arguments are present but match neither cleanly. Ask the user which
  they meant and wait for the answer before spawning anything.

If `$ARGUMENTS` is empty, default to diff scope against the repository's default branch and
state that assumption in your first message to the user.

### 0b — Resolve the model

Ask the user which model the sub-agents should use (default: `opus` if they have no
preference), unless the caller already supplied one. Refer to it below as `$SECURITY_MODEL`.

### 0c — Detect the project profile

Inspect the in-scope code and its manifests, and pick the profile (or profiles) that fit.
This decides which checklist sections the `gen-reviewer` lane applies and whether the
`crypto-reviewer` lane runs at all.

| Profile | Signals |
|---|---|
| `web-service` | HTTP framework deps (axum, actix, express, fastify, next, flask, django, gin), route/handler definitions, middleware, templates |
| `library-ffi` | lib/cdylib targets, `extern "C"`, `#[no_mangle]`, `#[repr(C)]`, `unsafe` blocks, published-package manifests, WASM bindings |
| `node-consensus` | p2p/networking stacks, consensus or block/transaction validation, wire deserialization of untrusted messages, crypto deps (curve25519, secp256k1, blake2/3, sha2, ring, rustls) |
| `cli-tooling` | bin targets, clap/argparse/cobra, subcommand dispatch, file and process handling, shell-out |

Record the profile(s) as `$PROFILE`. If more than one fits, list them all; if none fit,
use `general` and say so. State `$PROFILE` and the evidence for it in your first message.

### 0d — Sanity-check before spawning

If diff scope resolves to zero changed files, or path scope resolves to paths that do not
exist, stop and tell the user. Agents run against an empty scope find nothing, and that
reads exactly like a clean report.

### 0e — Preflight requirements

If the caller says the preflight is already done, skip to 0f.

Otherwise check every requirement in `$SKILL_DIR/requirements.md` that applies to this
scope, and ask the user to install whatever is missing, as that file describes. Record the
requirements the user chose to continue without as `$UNAVAILABLE` (or `none`).

### 0f — Index the codebase

Index the codebase yourself with the `cocoindex-code:ccc` skill, once, and wait for the index
to finish before Wave 1 starts. Every lane searches this one index.

Report whether the index was already current or had to be rebuilt. If indexing fails, say so
and continue: the lanes fall back to direct file reads and grep. Record the degraded
navigation in the final report's coverage gaps.

### 0g — Build the shared preamble

Assemble this `$PREAMBLE` block and prepend it verbatim to **every** sub-agent's
instructions in Steps 1 and 2. It is what makes the lanes comparable and de-duplicable.

```
SCOPE
Scope type:   diff | path
Base ref:     <the merge-base ref, or "n/a" for path scope>
Profile:      <$PROFILE>
Unavailable:  <$UNAVAILABLE — skills and tools missing for this run; use the fallback your
              brief gives for each, and state at the top of your report that you did>
In scope:     <explicit list of files or paths; if long, list the directories and the file count>
Out of scope: tests, test fixtures, examples, benchmarks, vendored dependencies, generated
              code, and code deliberately marked unsafe for demonstration — unless one of
              these is explicitly named in "In scope" above.

RULES
- Review only: report findings and leave every file, the index, and git history unchanged.
- Search the existing cocoindex-code:ccc index (already current) to find call sites,
  callers, and related code. Trace each suspect value back to its source and forward to
  its sinks.
- Report only issues reachable from code in scope. If no caller or input you can identify
  reaches a code path, say so and drop it.
- Ignore coding nits: style, naming, formatting, preference-level refactors. Report
  everything else, including Low severity, with a severity attached.
- If you cannot complete a check — a tool is missing, a file is unreadable, the scope is
  unclear — say so explicitly in your report. A skipped check and a clean check must never
  look the same.

FINDING FORMAT — use exactly these fields for every finding:
- Title:         one line
- Severity:      Critical | High | Medium | Low
- CWE:           CWE-### and name, or "n/a"
- Location:      file:line for every affected site (list them all, not just the first)
- Preconditions: what an attacker needs before this is exploitable (access, role, timing, config)
- Attack:        concrete steps from those preconditions to the impact
- Impact:        what the attacker gains or the system loses
- Remediation:   what to change
- Confidence:    High | Medium | Low, and what would raise it

If you find nothing, say so plainly and list what you checked.
```

## Step 1 — Wave 1: spawn the scan lanes in parallel

Spawn every lane below **in a single message** so they run concurrently. Each is a
`general-purpose` agent on `$SECURITY_MODEL`; the names are labels for your own tracking
and the final report, not registered agent types.

Each lane's instructions are `$PREAMBLE`, then: "Read `$SKILL_DIR/lanes/<file>` and carry
out every check in it." (with `$SKILL_DIR` filled in as an absolute path).

| Lane | Brief | Runs |
|---|---|---|
| `security-reviewer` | `security-review.md` | always |
| `gen-reviewer` | `appsec.md` | always — it applies the sections matching `Profile` |
| `dep-reviewer` | `deps-secrets.md` | always |
| `crypto-reviewer` | `crypto-consensus.md` | unless the scope has no cryptographic operations, no consensus or validation logic, and no parsing of untrusted data — if skipped, say why |
| `secskill-reviewer` | `threat-model.md` | always |

Report each lane to the user as it lands, including lanes that fail, error, or return
nothing. Name the lane, whether it succeeded, and how many findings it produced.

## Step 2 — Wave 2: attack-path chaining

Wait for all of Wave 1, then spawn one more `general-purpose` agent, `attack-path-reviewer`,
on `$SECURITY_MODEL`. It runs second because it consumes the other lanes' output.

Its instructions are `$PREAMBLE`, then the raw findings from every Wave 1 lane, then:
"Read `$SKILL_DIR/lanes/attack-path.md` and carry it out."

## Step 3 — Verify before reporting

Multi-agent security scans produce confident-sounding findings that the code does not
support, and an unverified report costs the user the time to disprove it. Treat every lane
finding as a claim.

For every finding, read the cited code yourself and mark it:

- **CONFIRMED** — you read the code at the cited location and it does what the finding says.
- **PLAUSIBLE** — the concern is real but you could not confirm reachability, exploitability,
  or the precondition. Keep it, and state exactly what you could not confirm.
- **DROPPED** — the code does not do what the finding claims, the path is unreachable, the
  input is already validated upstream, or a compensating control the lane did not see
  handles it. Remove it from the report and note the count of dropped findings.

If there are more than ten findings, you may spawn verification agents in parallel — one
per batch of findings, on `$SECURITY_MODEL`, each given the findings and the standard
preamble, and each instructed to re-read the cited code rather than trust the claim.

A finding is CONFIRMED only once the cited code has been read — by you or a verification
agent — and shown to do what it claims.

## Step 4 — Aggregate and report

De-duplicate:

- **Merge** findings that share a root cause, even when they cite different call sites.
  One finding, with every affected location listed in the Location field, and the highest
  severity of the merged set.
- **Keep separate** distinct defects that happen to live in the same file or function.
- When merging, keep the clearest attack scenario and union the affected locations.

Then produce a single report containing:

1. **Header** — the resolved scope, the detected `$PROFILE`, the model used, which lanes ran,
   which lanes failed or were skipped and why, and the count of findings dropped in Step 3.
2. **Choke points** — the fixes from Step 2 that break the most attack chains, first.
3. **Findings** — sorted Critical, then High, then Medium, then Low. Every finding in the
   standard format, each tagged CONFIRMED or PLAUSIBLE and with the lane that found it.
   Report every severity; the only thing filtered out is coding nits.
4. **Coverage gaps** — everything in `$UNAVAILABLE` and the fallback used for it, checks
   that could not be run, and areas the scope excluded, so the user knows what this report does not cover.

If nothing survived verification, report that all is fine — and still include the header and
the coverage gaps, so "clean" is distinguishable from "not actually checked".
