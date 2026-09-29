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

This workflow is **review-only**. Neither you nor any sub-agent modifies code, stages, or
commits. Fixing is a separate, explicit step the user invokes afterwards.

## Step 0 — Resolve scope, model, and project profile before spawning anything

### 0a — Resolve the review scope

Turn `$ARGUMENTS` into one concrete, written-out scope. Do not proceed with a vague
scope like "the new code": every sub-agent below is handed this scope verbatim, and an
unresolved scope is the most common way this workflow silently reviews the wrong thing —
or nothing at all.

Classify `$ARGUMENTS` into exactly one of:

- **Diff scope** — the arguments mention a branch, a diff, a PR, "my changes", or "the new
  code", or the arguments are empty. Resolve it by finding the base ref and the changed
  files: `git merge-base HEAD <default-branch>`, then `git diff --stat <base>..HEAD`, plus
  `git status --porcelain` for uncommitted work.
- **Path scope** — the arguments name one or more files, directories, crates, or packages.
  Resolve each to an actual path on disk and confirm it exists.
- **Ambiguous** — the arguments are present but match neither cleanly. Ask the user which
  they meant and wait for an answer. Do not spawn anything until the scope is settled.

If `$ARGUMENTS` is empty, default to diff scope against the repository's default branch and
state that assumption in your first message to the user.

### 0b — Resolve the model

Ask the user which model the sub-agents should use (default: `opus` if they have no
preference). Refer to it below as `$SECURITY_MODEL`.

### 0c — Detect the project profile

Inspect the in-scope code and its manifests, and pick the profile (or profiles) that fit.
This decides which checklist Agent 2 runs and whether Agent 4 runs at all — a web-app
checklist aimed at a Rust node burns a whole lane finding nothing.

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
exist, stop and tell the user. Never spawn agents against an empty scope — five agents
finding nothing in nothing reads exactly like a clean report.

### 0e — Index the codebase

Before spawning anything, index the codebase yourself with the `cocoindex-code:ccc` skill,
and wait for the index to finish.

Do this once, here, in the orchestrator. Every lane below searches that index, so it has to
be current before Wave 1 starts — and five agents each deciding whether to re-index the same
repository concurrently is both wasted work and a race over shared index state.

Report whether the index was already current or had to be rebuilt. If indexing fails, say so
and continue: the lanes fall back to direct file reads and grep, but note the degraded
navigation in the final report's coverage gaps, since it affects how much of the call graph
the agents could actually follow.

### 0f — Build the shared preamble

Assemble this `$PREAMBLE` block and prepend it verbatim to **every** sub-agent's
instructions in Steps 1 and 2. It is what makes the lanes comparable and de-duplicable.

```
SCOPE
Scope type:   diff | path
Base ref:     <the merge-base ref, or "n/a" for path scope>
Profile:      <$PROFILE>
In scope:     <explicit list of files or paths; if long, list the directories and the file count>
Out of scope: tests, test fixtures, examples, benchmarks, vendored dependencies, generated
              code, and code deliberately marked unsafe for demonstration — unless one of
              these is explicitly named in "In scope" above.

RULES
- Review only. Do not edit, create, stage, or commit any file. Report; do not fix.
- The orchestrator has already indexed this codebase with the cocoindex-code:ccc skill, and
  the index is current. Do NOT re-index — search the existing index to find call sites,
  callers, and related code rather than grepping blind. Trace each suspect value back to its
  source and forward to its sinks.
- Only report issues reachable from code in scope. If a code path cannot be reached by any
  caller or input you can identify, say so and drop it rather than reporting it as theoretical.
- Ignore coding nits: style, naming, formatting, preference-level refactors. Report
  everything else, including Low severity, with a severity attached.
- If you cannot complete a check — a tool is missing, a file is unreadable, the scope is
  unclear — say so explicitly in your report. Do not silently skip it. A skipped check and
  a clean check must never look the same.

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

Spawn the following agents **in a single message** so they run concurrently — they are
independent and serialising them costs five times the wall-clock for nothing.

Each is a `general-purpose` agent using `$SECURITY_MODEL` as its model. The names below
(`security-reviewer`, `gen-reviewer`, and so on) are labels for your own tracking and final
report — they are not registered agent types.

Prepend `$PREAMBLE` to every set of instructions.

Report each lane to the user as it lands, including lanes that fail, error, or return
nothing. Name the lane, whether it succeeded, and how many findings it produced.

### Agent 1 — Security review

Spawn `security-reviewer` with these instructions:

```
  Do a security review of the code in scope.

  If the scope type is `diff`: use /security-review — it reviews the pending changes on the
  current branch, which is what this scope describes.

  If the scope type is `path`: do NOT rely on /security-review. It only inspects pending
  branch changes and will silently miss everything in the paths listed above. Review the
  in-scope files directly instead.

  Report back all findings.
```

### Agent 2 — Application security review

Spawn `gen-reviewer` with these instructions. Include **only** the checklist sections
matching `$PROFILE`, plus the "All profiles" section:

```
  Think like a red-team attacker for every piece of code in scope.

  All profiles:
  - Input validation: is every externally-controlled input validated before use, at the
    boundary, against an allow-list rather than a deny-list?
  - Injection: is input ever interpolated into SQL, shell commands, HTML, file paths,
    format strings, log lines, or regexes?
  - Secrets: any credentials, keys, or tokens hardcoded? Any new env var undocumented?
  - Error handling: do errors leak stack traces, internal paths, or query text to callers?
  - Logging: is sensitive data (tokens, keys, PII, full request bodies) written to logs?

  web-service:
  - IDOR: can User A reach User B's data by changing an ID? Check every route with an id param.
  - Auth: is every new endpoint behind auth middleware? Are permissions checked, not just
    authentication? Is the check server-side and not inferable from the client?
  - Session management, CSRF protection, cookie flags (HttpOnly, Secure, SameSite).
  - XSS via unescaped input in templates or client-rendered HTML.
  - Data exposure: do responses leak internal fields — password hashes, internal IDs,
    other users' records, soft-deleted rows?
  - Configuration: debug mode, permissive CORS, missing security headers, verbose errors.
  - Rate limiting on auth, password reset, and expensive endpoints.

  library-ffi:
  - Every `unsafe` block: is there a SAFETY comment, and is the invariant actually upheld?
  - FFI boundary: memory allocated by C is freed by C; no Rust memory handed to foreign
    code without correct layout; no dangling pointers across the boundary.
  - Raw pointer arithmetic, transmute, and casts — check for UB and provenance violations.
  - Panic across an FFI boundary is UB: is unwinding caught at the boundary?
  - Public API surface: can a caller trigger UB using only safe API? That is a soundness bug.
  - Input from untrusted callers: lengths, null-termination, alignment, aliasing.

  cli-tooling:
  - Shell-out: is any argument attacker-influenced? Prefer argv arrays over shell strings.
  - Path traversal in file arguments; symlink following; TOCTOU between check and open.
  - Temp file creation: predictable names, missing O_EXCL, world-readable permissions.
  - Environment and config file trust: does the tool execute or load anything it finds?
  - Credentials passed as command-line arguments (visible in ps) or written to shell history.

  node-consensus:
  - Handled primarily by Agent 4. Report only what Agent 4's brief does not cover.

  Report back all findings.
```

### Agent 3 — Dependency and secret audit

Spawn `dep-reviewer` with these instructions:

```
  This lane runs tools and reports what they actually output. Do not reason about
  dependencies or secrets from memory — run the commands, paste the relevant output, and
  if a tool is not installed, say so explicitly rather than skipping the check silently.

  Dependencies — run whichever match the project's manifests:
  - Rust:   `cargo audit`, and `cargo deny check advisories bans sources` if deny.toml exists
  - Node:   `npm audit --audit-level=low` (or `pnpm audit` / `yarn npm audit` to match the lockfile)
  - Python: `pip-audit`, or `safety check` if pip-audit is unavailable
  - Go:     `govulncheck ./...`
  - Ruby:   `bundle audit check --update`

  Then, regardless of ecosystem:
  - Diff the lockfile against the base ref. Flag every newly added or version-bumped
    dependency, and check each new name for typosquatting against the package it resembles.
  - Flag dependencies pulled from a git URL, a local path, or a non-default registry.
  - Flag install/postinstall/build scripts in newly added packages.
  - Flag unmaintained or yanked packages, and direct dependencies more than one major
    version behind a version that fixes a known advisory.

  Secrets — scan both the working tree and the history:
  - `gitleaks detect --no-banner` over the tree, and over history if the scope is a diff
  - `trufflehog filesystem <path>` if available
  - If neither tool is installed, fall back to pattern and entropy search for API keys,
    private key headers, JWTs, connection strings, and cloud credentials — and state in
    your report that you fell back, so the coverage gap is visible.
  - Check history for secrets that were committed and later removed:
    `git log -p -S'<candidate>'` for each candidate, and scan the diff of any file whose
    name suggests credentials (.env, *.pem, *.key, credentials*, *secret*).
    A secret deleted in a later commit is still a live secret — report it as such.

  Report each vulnerable dependency as a finding using the standard format, with the
  advisory ID in the CWE field and the dependency path in Location.
```

### Agent 4 — Cryptography, consensus, and untrusted input

Spawn `crypto-reviewer` with these instructions. **Skip this lane only if** the scope
contains no cryptographic operations, no consensus or validation logic, and no parsing of
data from an untrusted source — if you skip it, say why in your report:

```
  Review the code in scope for cryptographic, consensus, and untrusted-input defects.
  These are not found by generic OWASP scanning, and they are usually Critical when present.

  Randomness and key material:
  - Is key, nonce, and salt material drawn from a CSPRNG (OsRng / getrandom), never from
    thread_rng-style convenience wrappers, a seeded RNG, or a time-derived seed?
  - Are secrets zeroized on drop? Do they appear in Debug/Display impls, serde output,
    error messages, or logs?

  Primitive misuse:
  - Nonce or IV reuse across messages under the same key; counter reuse after restart.
  - Non-constant-time comparison of secrets, MACs, or auth tags — flag `==` on secret
    bytes; expect a constant-time comparison instead.
  - Missing domain separation when hashing structurally different inputs into one function.
  - Signature malleability; signatures that do not cover every security-relevant field;
    verification results that are computed but never checked.
  - Home-rolled crypto where a vetted primitive exists.

  Arithmetic:
  - Overflow and underflow in amount, balance, fee, weight, and index arithmetic. In Rust,
    release builds wrap silently — flag bare +, -, * on value-carrying types and expect
    checked_/saturating_ with the saturation case justified.
  - Precision loss from float or lossy integer casts anywhere value is computed.
  - Division or modulo by a value an attacker can drive to zero.

  Denial of service from untrusted input:
  - Panic-as-DoS: unwrap, expect, direct indexing, slicing, and integer division applied to
    attacker-controlled data on a network or validation path. In a node, a reachable panic
    is a remote crash.
  - Unbounded allocation: `with_capacity` or `resize` driven by an attacker-supplied length
    field; `collect` over an untrusted iterator; missing maximum message and field sizes.
  - Deserialization without length caps or recursion-depth limits; decompression bombs.
  - Unbounded work: loops, hashing, or signature verification whose iteration count comes
    from the input.

  Protocol and consensus:
  - Replay protection: are nonces, timestamps, chain/network IDs, and domain tags included
    in signed and authenticated payloads?
  - Non-determinism on a consensus path: HashMap/HashSet iteration order, float arithmetic,
    system time, thread scheduling, or locale-dependent behaviour.
  - Trust in peer-supplied timestamps, heights, or difficulty without bounds.
  - Validation order: is anything expensive done before the cheap authenticity check?

  Report back all findings.
```

### Agent 5 — Threat model

Spawn `secskill-reviewer` with these instructions:

```
  Use the senior-security skill to STRIDE threat-model the code in scope.
  Build a data-flow view of the in-scope code, identify the trust boundaries it crosses,
  enumerate threats per STRIDE category (Spoofing, Tampering, Repudiation, Information
  disclosure, Denial of service, Elevation of privilege), and DREAD-score each one.
  Stay at the design level — the other sub-agents already scan for implementation bugs, so
  report architectural and trust-boundary threats, not duplicate findings.
  Report back all findings.
```

## Step 2 — Wave 2: attack-path chaining

Wait for all of Wave 1, then spawn one more `general-purpose` agent, `attack-path-reviewer`,
on `$SECURITY_MODEL`. This lane runs second because it consumes the other lanes' output.

Prepend `$PREAMBLE`, then the raw findings from every Wave 1 lane, then:

```
  Use the red-team skill. You are not looking for new individual bugs — the lanes above
  already did that. Your job is to chain them.

  1. Take the findings above as a starting inventory of attacker capabilities.
  2. Build the attack paths: which findings compose? A Low-severity information disclosure
     that supplies the precondition for a Medium-severity auth weakness is a High-severity
     chain, and no single-issue lane can see it.
  3. For each chain, give the entry point, the ordered steps, the findings it uses, the
     end state, and the severity of the chain as a whole.
  4. Identify choke points: the single fix that breaks the most chains. Call these out
     explicitly — they are the highest-value remediations in the report.
  5. Flag any capability a chain needs that no lane reported. That gap is itself a finding:
     either the capability exists and was missed, or the chain is broken. Say which.

  Report chains using the standard finding format, with the component findings listed in
  the Attack field.
```

## Step 3 — Verify before reporting

Do not pass the lanes' reports through untouched. Multi-agent security scans produce
confident-sounding findings that the code does not support, and an unverified report is
worse than no report because it costs the user the time to disprove it.

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

Never mark a finding CONFIRMED on the strength of the reporting agent's description alone.

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
4. **Coverage gaps** — checks that could not be run, tools that were missing, and areas the
   scope excluded, so the user knows what this report does not cover.

If nothing survived verification, report that all is fine — and still include the header and
the coverage gaps, so "clean" is distinguishable from "not actually checked".
