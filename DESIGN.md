# claude-web — local multi-agent Claude Code server

A local Rust web server that spawns and supervises multiple long-lived Claude Code
agents. Each agent owns a task, a name, a git branch and an isolated worktree.
A browser portal lists every agent with live status and lets you drive any one of
them like a terminal session.

Status: **design agreed, not yet implemented.**

---

## 1. Verified protocol facts

Everything below was tested locally against `claude` **2.1.241** (macOS, arm64).
These are load-bearing — several contradict what the published docs imply.

| # | Fact | Evidence |
|---|------|----------|
| F1 | `claude -p --input-format stream-json --output-format stream-json` is a **long-lived, multi-turn process**. It does *not* exit after one turn; it waits on stdin. Same `session_id` throughout, one `result` event per turn, a fresh `system/init` before each turn. | Two messages 30s apart, one process, both answered. |
| F2 | A non-SDK host **can intercept tool permissions**. `--permission-prompt-tool stdio` (undocumented, absent from `--help`, accepted) makes the CLI emit `control_request` / `can_use_tool` carrying `tool_name`, `display_name`, `input`, `description`, `permission_suggestions`, `tool_use_id`. Host replies with a `control_response` `{behavior: "allow"｜"deny", updatedInput}`. | Answered `allow` over stdin; file was written. |
| F3 | Without a handler, `--permission-mode manual` **denies silently** — `system/permission_denied` plus an error tool_result. No prompt is ever surfaced. | Write blocked, agent reported it couldn't ask. |
| F4 | Trivially safe commands (e.g. `echo hello`) are **auto-approved** by an internal classifier even in `manual` mode. Not every tool call produces a `can_use_tool`. | Bash echo ran unprompted. |
| F5 | **Interrupt is a control request**, not a signal: `{"type":"control_request","request_id":X,"request":{"subtype":"interrupt"}}` → `control_response` `{"still_queued":[...]}`. Session survives. | Sent mid-session; agent answered normally afterwards. |
| F6 | The CLI **queues** messages received during a turn — hence `still_queued` and the advertised `interrupt_cancel_queued_v1`. | Capability list + interrupt response shape. |
| F7 | **Resume works with stream-json.** Launch with `--session-id <uuid>`, relaunch with `--resume <uuid>`; conversation context survives process death. | Killed process; new one recalled a word from before. |
| F8 | **Skills / plugin / custom slash commands resolve headless.** Built-in TUI commands do not (`/status` → "isn't available in this environment"). | `/mattpocock-skills:ask-matt` loaded and answered in-voice. |
| F9 | The `initialize` control request returns the **full command list** with `name`, `description`, `argumentHint`. | `control_response` inspected. |
| F10 | `init` advertises `capabilities: [interrupt_receipt_v1, interrupt_cancel_queued_v1, msg_lifecycle_v1]`. | Observed on every launch. |
| F11 | On-disk transcripts (`~/.claude/projects/*/*.jsonl`) are **documented as an unstable internal format**. Not a store we may read. | Official docs. |
| F13 | The CLI emits a **`rate_limit_event`** whenever the account's usage changes, in practice once per API request. `rate_limit_info` is camelCase inside a snake_case envelope: `status` (`allowed`｜`allowed_warning`｜`rejected`), and optionally `resetsAt` (unix **seconds**), `rateLimitType`, `utilization`, `isUsingOverage` and `unifiedWindows` (`five_hour`, `seven_day`, `seven_day_overage_included`, each `{utilization, resetsAt}`). Present since at least 2.1.241. | Captured from 2.1.241 and 2.1.246; shape cross-checked against the CLI's own schema. |
| F14 | The CLI emits a **`tool_progress`** every 30s for any tool still running (`heartbeat: true`), carrying `tool_name`, `elapsed_time_seconds`, and two ids that are not what they look like: `tool_use_id` is **synthetic** (`<real tool_use_id>-heartbeat-<n>`, so it matches no `tool_use` block) and `parent_tool_use_id` is the **tool actually running**, not a nesting flag. A variant with no heartbeat carries `subagent_retry` while a subagent's API call is being retried. The `bash_progress`-derived variant, which would carry incremental output, is gated behind `CLAUDE_CODE_REMOTE`/`CLAUDE_CODE_CONTAINER_ID`, so a local child never sends it. | Captured from 2.1.246: a 95s foreground `Bash` yielded `toolu_01Xd…-heartbeat-0` with `parent_tool_use_id` = `toolu_01Xd…`. 30s interval read from the CLI's own timer. |
| F14a | The Agent tool is **exempt** from heartbeats, and a tool running *inside* a subagent was not observed to emit `tool_progress` on the parent's stream at all — so a long subagent is invisible to this signal from both directions. Stated as an observation, not a guarantee. | A delegated 40s `Bash` inside a `general-purpose` subagent produced no `tool_progress`; the exemption is explicit in the CLI's heartbeat timer. |
| F12 | Remote Control (`claude remote-control`) is a persistent server, ≤32 concurrent sessions, outbound-only, but **scoped to one directory** and requiring a full-scope subscription login. Compatibility with `-p` is **undocumented and untested** (testing would create a real session on the account). | `claude remote-control --help` + docs. |

### Risk register

- **`--permission-prompt-tool` is a hidden flag** and the stream-json protocol carries no
  stability guarantee. Mitigation: pin a known-good CLI version in config, assert on
  `system/init` at startup, log-and-surface unrecognised event types rather than crashing,
  and keep every raw event in the DB so nothing is lost to a parser gap.
  That fallback is a safety net, not a resting place: an event type seen in practice gets a
  parser arm. `rate_limit_event` and `tool_progress` (F13, F14) are gauges rather than
  transcript entries, so both are handled without a row in `events` and without a toast —
  persisting a 30s heartbeat buries the transcript it is meant to annotate.
- **F4 means the approval UI is not a complete audit trail.** Some tool calls execute without
  ever asking. The transcript shows them; the approval queue does not.

---

## 2. Architecture

```
browser ──WebSocket──┐
                     ├── axum (127.0.0.1:7717) ── supervisor ──┬── claude #1  (stdio, cwd=worktree A)
browser ──HTTP GET───┘            │                            ├── claude #2  (stdio, cwd=worktree B)
                                  │                            └── claude #N
                             SQLite (agents, events)
```

One `claude` child per agent. The supervisor owns each child's stdin/stdout, parses the
stream-json line protocol, persists events, updates status, and fans out to any connected
browsers. Pure Rust — no Node or Python sidecar (F2 is what makes this possible).

### Crates
`tokio` · `axum` (ws) · `rusqlite` (bundled) · `rust-embed` · `serde`/`serde_json` ·
`tracing` · `clap` · `uuid` · `nix` (SIGTERM) · `open` (browser launch) · `anyhow`

### Module layout
```
src/
  main.rs           CLI args, startup, graceful shutdown
  config.rs         config.toml load/save
  db.rs             schema, migrations, event append, cursor queries
  agent/
    supervisor.rs   registry, spawn/stop/resume, concurrency cap
    process.rs      child lifecycle, stdin writer, stdout reader, SIGTERM
    protocol.rs     stream-json event + control_request/response types
    state.rs        status state machine
  repo/
    scan.rs         repo-root scanning, git metadata, recency ordering
    git.rs          branch naming, worktree add/remove, safety checks
    clone.rs        git clone with GIT_TERMINAL_PROMPT=0
  web/
    routes.rs       REST endpoints
    ws.rs           multiplexed socket
  assets/           embedded HTML/CSS/JS
```

---

## 3. Data model (SQLite, `rusqlite` bundled)

```sql
CREATE TABLE agents (
  id                TEXT PRIMARY KEY,      -- uuid, also the claude --session-id
  name              TEXT NOT NULL,
  slug              TEXT NOT NULL UNIQUE,  -- URL identity
  repo_path         TEXT NOT NULL,         -- repo the task belongs to
  work_path         TEXT NOT NULL,         -- worktree, or repo_path if in-place/non-git
  is_git            INTEGER NOT NULL,
  branch            TEXT,                  -- NULL for non-git folders
  base_ref          TEXT,                  -- NULL when an existing branch was reused
  uses_worktree     INTEGER NOT NULL,
  branch_is_new     INTEGER,               -- we created `branch`, so delete may drop it
  permission_mode   TEXT NOT NULL,         -- ask | acceptEdits | bypass | dangerous
  model             TEXT,
  effort            TEXT,
  max_budget_usd    REAL,
  status            TEXT NOT NULL,
  status_detail     TEXT,                  -- e.g. "Bash: cargo test — 1m30s"
  exit_code         INTEGER,
  last_stderr       TEXT,
  cost_usd          REAL NOT NULL DEFAULT 0,
  created_at        INTEGER NOT NULL,
  last_active_at    INTEGER NOT NULL
);

CREATE TABLE events (
  agent_id  TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  seq       INTEGER NOT NULL,              -- monotonic per agent; the replay cursor
  ts        INTEGER NOT NULL,
  kind      TEXT NOT NULL,                 -- user|assistant|tool_use|tool_result|
                                           -- permission_request|permission_decision|
                                           -- system|result|stderr|error
  payload   TEXT NOT NULL,                 -- raw JSON verbatim, always
  PRIMARY KEY (agent_id, seq)
);

CREATE TABLE repo_usage (path TEXT PRIMARY KEY, last_used_at INTEGER NOT NULL);
```

**Implementation note — one additive migration.** The shipped schema carries one
column beyond the table above: `agents.add_dirs` (a JSON array, added by an
`ALTER TABLE` guarded on `PRAGMA table_info`, so an existing database opens
unchanged). Without it the `--add-dir` values chosen at spawn are lost on
Resume after a server restart, which silently changes what the agent can reach.
Everything else matches this schema exactly.

**Not persisted:** `stream_event` partial-token deltas. `--include-partial-messages` drives
live typing in the UI, but only completed blocks are written — otherwise the table grows by
thousands of rows per turn and replay re-animates every keystroke.

---

## 4. Agent lifecycle

### Status state machine
`Starting → Idle → Working → Idle …`, with `AwaitingApproval` branching off `Working`,
and `Stopped(code)` / `Failed(error)` as terminal states. `Working` carries a live
sub-label naming the current tool.

### Launch
```
claude -p
  --input-format stream-json --output-format stream-json --verbose
  --include-partial-messages
  --permission-prompt-tool stdio
  --permission-mode <manual|acceptEdits|bypassPermissions>
  --session-id <uuid>            # first launch
  [--resume <uuid>]              # subsequent launches, replaces --session-id
  [--model X] [--effort X] [--max-budget-usd X] [--add-dir ...]
cwd = work_path
```
`Dangerously skip all` substitutes `--dangerously-skip-permissions` for `--permission-mode`.

### Verbs
| Verb | Effect |
|---|---|
| **Interrupt** | `control_request`/`interrupt` (F5). Cancels the in-flight turn **only**; reports `still_queued`. Separate "clear queue" action. |
| **Stop** | SIGTERM (runs `SessionEnd` hooks, records the interrupted turn), 5s grace, then SIGKILL — to the CLI's process group *and* to every other process group found under it (see below). History and `session_id` retained. Worktree untouched. |
| **Resume** | Respawn with `--resume <session_id>` (F7). |
| **Delete** | Removes agent + events. Worktree safety check first — see §6. |
| **Rename** | Display name only; slug and branch are immutable. |

**No auto-restart.** An unexpected exit surfaces `Failed` with exit code and last stderr,
and offers one-click Resume. Silent respawn duplicates side effects.

### What a stop can and cannot reach

The child is spawned in its own process group, so `killpg` reaps it and anything
that stays in that group. That is not enough on its own: **`claude` runs each
Bash tool call in a *new* process group**, so a `cargo build` or `npm run dev`
started by a tool call is invisible to `killpg` on the CLI's group. Verified
live against `claude` 2.1.241 — the CLI led group *N*, its Bash descendant led
group *M*, unrelated to *N*.

So the supervisor keeps a running list of those groups instead of trying to find
them after the fact. The process tree under the CLI is snapshotted (`ps -axo
pid,ppid,pgid,etime`, walked breadth-first with a visited set, in
`spawn_blocking`) several times in the first quarter-second of each tool call,
again when it returns or the turn ends, and every couple of seconds while the
agent is otherwise busy and there is something to keep track of. The groups found
are accumulated over the session; a group is dropped only once nothing of it is
left, never because it is old, because a group recorded early and still running
is precisely the one worth keeping.

The early samples in that first quarter-second are a cheap best effort at
catching a tool call whose shell exits immediately. They are kept because they
cost little and occasionally win, but they are **not** a fix for backgrounded
jobs — see the measurements below.

Nothing is signalled on the strength of a group id alone, and nothing is
*adopted* on the strength of a descendant merely being in a group. The server
does not call `setsid`, so it, its agents and — when launched from a terminal —
the operator's shell share one session, and any process may `setpgid` itself
into any group in its session. So a group is only ever recorded when its
**leader** is a descendant of the CLI, pinned by that leader's pid *and* start
time; another live agent's group is excluded outright. Without that, one agent
could have the server SIGKILL the operator's editor by joining its group for a
moment.

A recorded group then counts as the agent's while its witness is still alive, or
while some live member **started before the last moment we proved the group was
ours** — which means the group has been non-empty ever since, and
a process group id cannot be handed to something else while its group still has
members. Pid identity alone is deliberately not proof: pids recycle too (macOS
wraps at 99998, and a session of parallel agents shelling out to builds churns
them), so a recorded pid can come back as something else entirely — which is why
the witness is pinned by start time as well. A zombie never counts as the group
being alive, and the whole-second granularity of `ps` is *subtracted* from the
proof window, never added, so a process that genuinely started after the last
proof can never carry it. Each snapshot
re-proves what it can and moves that timestamp forward, which is what keeps a
`npm run dev` reachable after it has forked workers and lost its original
parent.

Teardown then runs on **every** exit path, not only an operator Stop: a crash, a
budget exhaustion, a non-zero exit. It SIGTERMs each group that still passes that
test, waits the 5s grace, and SIGKILLs whatever is left. It is awaited inside the
runner, so the agent is not deregistered — and the server is not allowed to
finish shutting down — until it has completed. Without that, shutdown returned as
soon as the CLI died and the escalation was cancelled with the runtime. pid 1,
the server's own pid and the server's own process group are never signalled.

**What is reliably swept, and what is not.** Measured against `claude` 2.1.241
on 2026-08-24, with an unrelated decoy process running throughout to check for
over-signalling:

| case | result |
|---|---|
| A tool call's child still running when the agent is **stopped** | swept |
| …when the CLI **crashes** (SIGKILL to the CLI) | swept |
| …when the **server shuts down** | swept |
| A job backgrounded with `&` inside a tool call (`sleep 1201 &`, `bash -c "sleep 1117 &"`) | **not swept** — 0 of 4 controlled trials, and at best 1 of 8 overall |
| An unrelated process of the user's, on any path | never signalled, ~10 trials (see the ownership rules below for what makes this hold against a *hostile* agent, which those trials did not test) |

The first three are the case this machinery exists for: a `cargo build`, an
`npm run dev`, anything still holding the worktree open at the moment the agent
goes away. Those are descendants of the CLI when we look, and they are reliably
caught.

A job backgrounded with `&` is, in practice, **not** caught, and no sampling
cadence fixes that. The tool's shell exits within a few milliseconds of starting
the job, and our first sample cannot run until we have already observed the
`tool_use` event — by which time the shell is usually gone and the job has
reparented to pid 1, in neither the CLI's process group nor its subtree. macOS
has no cgroup or PID-namespace equivalent to recover the relationship after the
fact. The same applies to `nohup`, to `setsid`, and to anything at all when the
server is killed with SIGKILL, which runs no teardown.

(An earlier stub-based measurement suggested the early samples caught the `&`
case 9 times out of 9. That stub reproduced the process *topology* but not the
*timing* — its shell lived long enough to be observed — so the figure said
nothing about real behaviour and is recorded here only so it is not mistaken for
evidence.)

The conservative ownership test above also means a group whose every live member
is younger than our last proof is left alone: leaving a build running is a
nuisance, signalling the user's editor is not. When a worktree removal is then
refused because something is still holding it, git's own stderr is surfaced to
the operator with the path and a "delete anyway" hint rather than a generic
failure.

### Shutdown
SIGTERM every child → 5s → SIGKILL stragglers, then the same escalation for any
process group the children's tool calls left running, awaited before the server
exits. All agents are marked `Stopped` so Resume works next boot.

### Concurrency
Soft cap, default **8**. Beyond it the spawn button warns; it does not block.

---

## 5. Permissions

Four per-agent modes chosen at spawn:

| UI label | Flags | Behaviour |
|---|---|---|
| **Ask me** (default) | `--permission-mode manual --permission-prompt-tool stdio` | `can_use_tool` → agent goes `AwaitingApproval`, browser shows tool, input and description, Approve / Deny. |
| **Accept edits** | `--permission-mode acceptEdits --permission-prompt-tool stdio` | Edits auto-approved; everything else still asks. |
| **Bypass** | `--permission-mode bypassPermissions` | No checks. |
| **Dangerously skip all** | `--dangerously-skip-permissions` | No checks, no guard rails. |

`permission_suggestions` from the request powers extra buttons ("Accept edits for this
session"), applied via the `set_permission_mode` control request.

Pending approvals block that agent only. They are persisted, so a browser reload doesn't
lose them. Caveat F4 applies: safe commands never appear here.

---

## 6. Repository picker, branching, worktrees

### Configuration
`config.toml` holds a **list** of repo roots (default `["~/Code"]`), editable from a
Settings page. Verified layout: `~/Code` has 46 directories — 39 git, 7 plain — completely
flat, no nesting.

### Scan
Immediate children only. Dot-directories skipped; symlinks followed but not recursed.
**All directories listed, not just git ones** — the 7 plain folders are real workspaces.
Each entry badged `git` (with current branch and dirty marker) or `plain`. Re-scanned on
every picker open.

### Ordering
A **Recent** group (max 5) ordered by *this tool's own* `last_used_at` descending, then
**All** alphabetically.

### Branching
- Slug from the task name (lowercase, non-alphanumerics → `_`, ≤40 chars).
- Prefix configurable, **default `sw_`**, matching the existing convention in these repos.
- Collision → `_2`, `_3`. Final name shown in the spawn form before launch.
- **Base ref = current HEAD**, shown in the form and overridable via dropdown.
  No automatic fetch — it's a button. (Default branches here are split `main`/`master`,
  and several repos deliberately sit on feature branches.)

### Worktrees
`git worktree add <repo_root>/.worktrees/<repo>/<slug> -b <branch>` — outside the repos,
under a dot-directory the scanner already skips. Each agent gets an isolated checkout, so
concurrent agents on one repo are safe and the main checkout is never touched.
A **Workspace** control on the spawn form chooses between that and the main checkout, where
the equivalent is `checkout -b`.

### Reusing an existing branch
A **Branch** control chooses between creating `<prefix><slug>` and checking out a branch
that already exists — `git worktree add -- <path> <branch>` or `git checkout <branch> --`.

Three rules keep reuse from becoming a way to damage someone else's work:

- **The name must already be a local branch.** Checked against `list_branches` by
  membership before any git command runs, so the reuse path can never *create* a branch —
  and an option-shaped name never reaches git at all.
- **No base ref.** A reused branch has its own head; a start point would move it. The
  request's `base_ref` is dropped rather than applied, and stored as `NULL`, so the
  delete-time check reports the branch's whole unpushed history — none of which this agent
  created.
- **The branch is never deleted with the agent.** `branch_is_new` records who created it;
  a reused branch is kept on delete whatever the request or the `force` flag says.

git's own refusals are surfaced, not forced past: a branch already checked out elsewhere
("`main` is already used by worktree at …") and a switch that would clobber uncommitted
changes both fail the spawn. Two checkouts of one branch is not isolation.

> **Warned in the UI:** a new worktree checks out from HEAD, so **uncommitted changes in the
> main checkout are invisible to the agent.** Spawning against a dirty repo shows a warning.

**Non-git folders:** spawn normally, no branch, badged "no VCS". Never `git init` implicitly.

### Cloning
URL field → folder name derived (editable) → `git clone --progress` into the chosen root,
progress streamed over the WebSocket, agent spawned on success.
`GIT_TERMINAL_PROMPT=0` and askpass disabled so it fails fast instead of hanging on
credentials; stderr surfaced with an "use an SSH URL" hint. Existing target directory →
refuse. No credential storage; ambient SSH agent and git credential helper only.

### Cleanup on Delete
**Never auto-commits, never auto-pushes.** Clean worktree whose branch is merged or empty →
remove worktree, offer to delete the branch. **Uncommitted changes or unpushed commits →
refuse**, show exactly what would be lost, require an explicit "delete anyway".
The branch survives Delete by default.

---

## 7. Web interface

Loopback only (`127.0.0.1:7717`), and a per-boot session token on every data
route — see **Authentication** below. The OS is not a sufficient boundary here:
the agents run on this machine too, and they are the principal the permission
modes exist to constrain. A non-loopback binding would need real multi-user auth
on top, and remains out of scope.

### Authentication

Loopback binding is not an authentication boundary. Everything on the machine
can reach the port — including the agents, which are the principal §5's
permission modes exist to constrain. An agent that gets one Bash call executed
could otherwise `POST /api/agents/<id>/permission_mode {"mode":"bypass"}` and
never be asked for approval again.

So the server mints a random 256-bit token at startup and requires it on every
`/api/*` request and on the `/ws` upgrade (a header, or the query string for the
upgrade alone, which browsers cannot add headers to). The pages and their assets
stay reachable without it so the browser can bootstrap; they contain no data. It
changes on every restart.

**What that achieves, and what it does not.** It stops anything that has not
been handed the token — a drive-by cross-origin request, and any local process
that does not go looking. It does **not** put the token out of reach of a
determined process running as the same user, and nothing can: the browser holds
it in a profile directory that is on disk and not privileged, so
`strings …/Local\ Storage/…` recovers it. The exposures the server itself
controls are kept small — never logged through `tracing`, never embedded in a
served page, printed only when stdout is a terminal (redirecting to a log file
would otherwise write it there), handed to the browser through a private 0600
file rather than a command line that every process can read, and held in
`sessionStorage` rather than `localStorage` so it does not outlive the browser
session. Treat this as raising the bar in front of the control plane, not as
closing it: an agent that goes looking in the browser profile can still recover
the token.

Relaxing an agent's permission mode is additionally a confirmed action, and the
change is written into that agent's own event log with its initiator, so it
appears in the transcript rather than silently.

### Host and Origin

Loopback binding alone does not survive DNS rebinding: a page served from
`http://evil.example:7717` and rebound to `127.0.0.1` is same-origin as far as
the browser is concerned. Every request is therefore checked for a loopback
`Host` header on the port actually being served, and any request carrying a
foreign `Origin` — WebSocket upgrades included — is refused with 403.

An **absent** `Origin` is not evidence of anything: a cross-origin no-cors GET
(`<img src>`, `<script src>`, a navigation) sends none, and its `Host` is
loopback because that is what the URL says. Data routes therefore also require
`Sec-Fetch-Site` to be `same-origin`/`none` where the browser sends it, and the
session token above regardless.

### Paths and subprocesses

Every caller-supplied path — the clone root, a spawn's repository, each
`--add-dir` — is canonicalised and required to sit inside a configured repo
root. Canonicalising is what makes it real: a symlink inside a root would
otherwise step outside it.

`git` honours the configuration of the directory it runs in, and many config
keys are command strings. A directory the user merely unzipped can therefore run
commands the moment the repo picker scans it — `git status` re-hashes files whose
stat data it cannot trust, and re-hashing runs the repository's own clean filter.

A fixed list of dangerous keys was tried twice and was incomplete both times:
the second attempt covered `core.fsmonitor` but not `filter.<name>.clean`, whose
name the attacker chooses. So before touching a repository, the configuration it
actually puts into effect is read and every command-valued key it declares is
disarmed by that exact name.

Reading it correctly took two more attempts, both caught by review:

* `git config --local --list` shows only what is literally in `.git/config`. A
  key reached through `include.path`, or living in `.git/config.worktree` behind
  `extensions.worktreeConfig`, was invisible to the guard while git honoured it.
  Both listings now pass `--includes`, and the per-worktree scope is read as a
  second listing.
* The read is scoped to the repository on purpose. An unscoped
  `git config --list` would pull in the operator's *global* config, where a
  normal `filter.lfs.clean` or `credential.helper` would be blanked — breaking
  git-lfs and authentication — and an unrecognised global key would refuse every
  repository they own.
* A repository whose config git will not parse is refused rather than run under
  the fixed list alone.

The disarming value is per key, and each was measured against a control that
first showed the command running. `core.hooksPath` is set to `/dev/null`;
blanking it works equally well, and an earlier claim here that blanking would
re-enable `.git/hooks` was wrong — it does not.

`remote.<n>.uploadpack` and `remote.<n>.receivepack` are the exception: `-c`
does not override them. Measured on git 2.52.0 with a control, the repository's
command ran under no override *and* under `-c remote.origin.uploadpack=` and
`-c …=git-upload-pack`, for `fetch --all`, `fetch origin` and `ls-remote`, over
both a bare path and a `file://` URL — even though `git config --get` showed the
override had landed. Only `--upload-pack=git-upload-pack` suppressed it, so
`fetch` pins that argument, and `fetch` is the only command here that contacts a
remote at all. (An independent re-run reported the opposite; the measurement
above is what reproduces here, and the pinned argument is safe either way.)

Anything command-shaped that we cannot prove inert makes the repository refuse
inspection outright, named in the message. The picker shows such a repository as
"not inspected" and spawning into it is refused.

The residual: a key that git executes, whose name matches none of the shapes we
know and does not end in a command-ish word, would still run — and it would run
under the fixed `SAFE_CONFIG` list, which stays as the backstop for exactly that
case. Clone URLs are restricted to https/ssh and absolute paths.

### Endpoints
```
GET  /                          dashboard
GET  /agent/:slug               detail view
GET  /api/repos                 scan + recency ordering
POST /api/repos/clone           clone into a root
GET  /api/agents                registry
POST /api/agents                spawn
POST /api/agents/:id/{interrupt|stop|resume|rename}
DEL  /api/agents/:id            with ?force=
GET  /api/agents/:id/events?after=<seq>
GET  /api/rate_limit            last usage snapshot (null until one arrives)
GET  /api/config, PUT /api/config
WS   /ws                        multiplexed, {agent_id, ...}-tagged envelope
```

One WebSocket serves both dashboard and detail views: one reconnect path, one schema.
Client→server: `send_message`, `permission_decision`, `interrupt`, `subscribe{agent_id, after_seq}`.
Server→client: `event`, `status`, `permission_request`, `partial`, `clone_progress`,
`rate_limit`.

`rate_limit` carries no `agent_id`: every agent's CLI reports the same account, so the last
snapshot to arrive is the truth for all of them. The supervisor keeps it so a page loaded
between two API calls has numbers to show, which is what `GET /api/rate_limit` serves.

### Replay
Cursor-based. The client holds its last-seen `seq`; on reconnect it sends the cursor and
receives only the delta. A fresh load gets the **last 500 events** with "load earlier".
Same query either way: `WHERE seq > ?`.

A page is capped at 500, so the reply carries `has_more`, and a client
reconnecting from an old cursor walks forward page by page until the server
stops saying there is more. That walk is driven purely by the server's replies:
the cursor it holds for rendering is a separate thing, because live events
advance it too and using it as the walk's termination test loses everything
between the page in flight and the head. Events are keyed by `seq` and inserted
in order, so replay and the live stream may overlap freely, and the cursor the
client reconnects with is the highest *contiguous* seq it holds — it never runs
ahead of a gap.

### Frontend
**No build step.** Hand-written HTML/CSS/vanilla ES modules embedded via `rust-embed`.
`cargo build` yields one self-contained binary; no npm, no bundler. Terminal-styled
monospace transcript pane — structured events underneath (tool calls as collapsible
blocks), terminal look on top.

### Usage panel
The dashboard shows one meter per rate-limit window — session (5 hours), week (7 days), and
the overage-included week where the account reports it — each with its utilization and when
it resets. Hidden entirely until a `rate_limit_event` has arrived, amber past 60% and red
past 90%. An account that reports no per-window breakdown still names a governing window,
which is shown instead of an empty panel.

### Spawn form
Repo picker (§6) · task name (auto-filled from folder, editable) · branch name preview ·
base ref · model · permission mode · optional first message.
**Advanced:** effort, `--add-dir`, `--max-budget-usd`, in-place-checkout toggle.

### Slash commands
Typing `/` opens an autocomplete fed by the `initialize` command list (F9): name,
description, argument hint. Known-unavailable built-ins greyed out (F8).

### Message queueing
The input stays live while `Working`; messages queue natively (F6) and render greyed with
an ✕ to cancel. Interrupt cancels the turn only and reports what remains queued.

---

## 8. Remote Control

**Not integrated.** The portal already does what Remote Control does — messages, streaming
output, approvals, interrupt — and keeps arbitrary per-agent directories, which Remote
Control cannot (it is scoped to one directory). Phone access comes from putting the portal
on Tailscale, or binding `0.0.0.0` behind a bearer token.

Deliberately avoided: an undocumented `-p` + `--remote-control` combination, a subscription
coupling, and two competing permission handlers (our stdio handler vs the phone).

A later escape hatch — a button that launches a *separate* `claude remote-control` server in
a chosen directory, with its own lifecycle and honestly labelled as not-our-agents — remains
possible. The design does not depend on it.

---

## 9. Configuration (`config.toml`)

```toml
port            = 7717
open_browser    = true
repo_roots      = ["~/Code"]
branch_prefix   = "sw_"
max_agents      = 8
default_model   = "opus"
default_permission_mode = "ask"
claude_bin      = "claude"
pinned_cli_version = "2.1.241"   # warn on mismatch
```

---

## 10. Out of scope (v1)

Multi-user auth · remote/non-loopback binding · auto-commit, auto-push, PR creation ·
Remote Control integration · reading Claude's internal transcript files (F11) ·
agents surviving server death · virtualised scrollback.

---

## 11. Note before implementation

The project directory is `~/Code/claude web` — **the space breaks `cargo init`'s default
package name.** Use `cargo init --name claude-web`.
