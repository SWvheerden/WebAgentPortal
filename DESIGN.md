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
| F1a | `system/init` opens a **turn**, not the process. A freshly launched child emits its `SessionStart` hook lines and answers control requests, then goes quiet — no `init` arrives until the first user message. So `init` is **not** a readiness signal; the reply to our `initialize` handshake (F9) is. | 2.1.247, both `--session-id` and `--resume`, no message sent: hook lines and the `initialize` reply within 1.3s, nothing further for 25s. |
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
| F14a | The Agent tool is **exempt** from heartbeats, and a tool running *inside* a subagent was not observed to emit `tool_progress` on the parent's stream at all — so a long subagent is invisible to this signal from both directions. Stated as an observation, not a guarantee. | A delegated 40s `Bash` inside a `general-purpose` subagent produced no `tool_progress`; the exemption is explicit in the CLI's heartbeat timer. The `task_*` lines of F17 are what fill that gap. |
| F15 | An API failure **does not stop the turn from looking like a success**. The CLI synthesises an `assistant` line with `is_api_error_message: true`, `error: "rate_limit"` and `model: "<synthetic>"` carrying the human text, then closes the turn with a `result` whose `subtype` is still **`"success"`** while `is_error: true` and `api_error_status: 429`. So `subtype` must never be keyed off; `is_error` is the flag that means it and `result` carries the wording. The agent is genuinely `Idle` afterwards — the process is alive and can be spoken to. | A session that hit its five-hour limit mid-task on 2.1.246: two synthetic `assistant` lines, then `result` with `subtype: "success"`, `is_error: true`, `api_error_status: 429`. |
| F16 | **Built-in TUI slash commands are not in the `initialize` list and are refused if typed.** `/resume`, `/status`, `/cost` and `/help` are absent from the 74 commands 2.1.247 advertised; sending `/resume` returns a `<synthetic>` assistant line — *"/resume isn't available in this environment."* — and a `result` with `is_error: false`. Skills, plugin and project commands do resolve (F8). | The advertised list inspected; `/resume` sent and refused. |
| F17 | The **Agent/Task tool is asynchronous**. The parent is handed its tool result — *"Async agent launched successfully … you will be notified automatically when it completes"* — the moment the subagent **starts**, and closes its turn with an ordinary `result` while the subagent runs on. The lifecycle arrives as `system` lines: `background_tasks_changed` (the **complete live list**, emptied when the last one ends), `task_started` (`task_id`, `tool_use_id`, `description`, `subagent_type`, `is_backgrounded`), `task_progress` (`description`, `last_tool_name`), `task_updated` (`patch.status`) and `task_notification` (`status`, `summary`). The notification then **wakes the parent into a fresh turn with no user message**. So `result` is not proof the agent has finished, and the first line of a turn is not always a reply to something we sent. | 2.1.251, one backgrounded `general-purpose` subagent: `task_started` → `result` ("I'll let you know as soon as it completes") → `background_tasks_changed: []` → `task_notification` → a second `system/init`, `assistant` lines and a second `result`, none of it prompted. |
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
- **A failed turn is indistinguishable from a finished one unless it is made so.** F15 means a
  rate limit ends the turn through the ordinary `TurnEnded` path and leaves the agent `Idle`,
  which is exactly what success looks like. The `result`'s `is_error` therefore raises an
  error notice naming the status and the CLI's own wording, once per turn; the synthetic
  `assistant` lines are filed under `error` rather than `assistant`, so the transcript never
  puts "You've hit your session limit" in Claude's mouth. Neither changes the state machine:
  the process really is idle and really can be spoken to.
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

-- One row: the account's last known usage, not an agent's. See "Usage panel".
CREATE TABLE rate_limit (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  captured_at INTEGER NOT NULL,
  payload     TEXT NOT NULL
);

-- Free-text reminders about work not being done now. Flat and global on purpose;
-- see "Notes".
CREATE TABLE notes (
  id         TEXT PRIMARY KEY,
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
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

**`Idle` claims the agent is waiting on the operator**, which is the only reason the
status exists — it is what tells them to type. By F17 the closing `result` no longer
proves it: a backgrounded subagent outlives the turn that launched it, and the CLI wakes
the agent when it reports. So a turn that ends with subagents still running **holds its
`TurnEnded` back** and shows `Working — waiting on subagent: <what it is doing>`; the
held turn end lands when the live list empties, so an agent the CLI decides not to wake
still comes to rest rather than sitting at `Working` for good. Only tasks the CLI calls
`is_backgrounded` are tracked — a synchronous subagent already keeps its parent inside
the tool call, and tracking one whose completion we might never see is the way to a
permanently busy agent.

And because that woken turn carries no message from us, **the first line of CLI output
raises `TurnStarted`**, whoever began the turn. Sending a message still raises it too,
so the transition arrives at most once per turn either way; a transition that lands
where it already was is not published.

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
`--permission-prompt-tool stdio` is passed in **every** mode, including the two that never
ask. It only says a handler is reachable; a `bypass` or `dangerous` launch carrying it still
runs tools without prompting (verified against the CLI). Omitting it is what would be
permanent: by F3, a mode later tightened to `manual` with no handler denies silently, so an
agent started in `bypass` could never be pulled back under review without a relaunch.

### Readiness: what moves an agent off `Starting`

The `initialize` control request is sent immediately after launch, and **its reply
is the readiness signal** — not `system/init`, which by F1a only arrives once a turn
begins. A first launch usually carries a first message, so `init` follows within a
second or two and the distinction never shows; a **resume sends no message at all**,
so keying readiness off `init` left every resumed agent displaying `starting` until
the operator happened to type something. An error reply counts: it still proves the
child is reading stdin and writing stdout, which is all `Idle` claims. A launch with
an empty first message had the same bug and is fixed by the same signal.

### Verbs
| Verb | Effect |
|---|---|
| **Interrupt** | `control_request`/`interrupt` (F5). Cancels the in-flight turn **only**; reports `still_queued`. Separate "clear queue" action. |
| **Stop** | SIGTERM (runs `SessionEnd` hooks, records the interrupted turn), 5s grace, then SIGKILL — to the CLI's process group *and* to every other process group found under it (see below). History and `session_id` retained. Worktree untouched. |
| **Resume** | Respawn with `--resume <session_id>` (F7). Restores the conversation; it does **not** restart the interrupted turn — the CLI comes up and waits, so continuing the work takes an ordinary message. `/resume` is not that message: it is a TUI command the headless CLI refuses (F16). |
| **Delete** | Removes agent + events. Worktree safety check first — see §6. |
| **Rename** | Display name only; slug and branch are immutable — the detail view's URL survives it. Offered on the dashboard card *and* in the agent window's header, and broadcast as `agent_renamed` so neither is left showing the old name. |

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

Four per-agent modes, chosen at spawn and changeable afterwards:

| UI label | Flags | Behaviour |
|---|---|---|
| **Ask me** (default) | `--permission-mode manual --permission-prompt-tool stdio` | `can_use_tool` → agent goes `AwaitingApproval`, browser shows tool, input and description, Approve / Deny. |
| **Accept edits** | `--permission-mode acceptEdits --permission-prompt-tool stdio` | Edits auto-approved; everything else still asks. |
| **Bypass** | `--permission-mode bypassPermissions --permission-prompt-tool stdio` | No checks. |
| **Dangerously skip all** | `--dangerously-skip-permissions --permission-prompt-tool stdio` | No checks, no guard rails. |

`permission_suggestions` from the request powers extra buttons ("Accept edits for this
session"), applied via the `set_permission_mode` control request.

### Changing the mode after the task has started

The mode an agent was spawned with is rarely the mode it should keep for a long task: work
that starts under review earns the right to run unattended, and work that has started
surprising the operator has to be pulled back under it. So the agent page carries the same
picker the spawn form does, on the agent's current mode, and `POST
/api/agents/<id>/permission_mode` applies it — live to a running agent through the
`set_permission_mode` control request, and at launch for a stopped one, which needs nothing
beyond the stored value.

Three rules hold it together. **Relaxing is confirmed** (§8) and written into the agent's own
event log with its initiator, so a widening appears in the transcript rather than silently.
**`Dangerously skip all` is launch-only** — it is a flag, not a runtime mode, so the server
refuses it for a running agent and the picker greys it out rather than recording a mode that
is not in force. And **the change is broadcast** as `permission_mode_changed`: the dashboard
shows each agent's mode too, and a stale one there reads as a promise the agent is not
keeping.

### A mode that skips the checks can still ask

`--permission-prompt-tool stdio` reaches the host in every mode, and one tool uses it
whatever the mode is: **`AskUserQuestion`** is the model asking the operator a question, not
asking to act, so it lands in the approval queue even under `bypass` and `dangerous`. Two
things follow. Answering may not be gated on the permission mode — doing so left those
agents stuck in `AwaitingApproval` with no way out; what may not be answered is a request
that is not outstanding, which the runner's pending map decides. And approving such a request
with the input untouched answers nothing: the CLI replies "the user did not answer the
questions" and the model asks again. The answer rides back in `updatedInput.answers`, keyed
by the question text and valued with the chosen label, so the browser renders the questions
and their options rather than an Approve button over a JSON dump.

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

### Leaving git alone
The **Branch** control's third setting, *stay on the current one*, runs no git command at
all: the agent works in the main checkout on whatever HEAD already is. It is the mode for
an agent that is not doing branch-shaped work — a question about the code, a one-file fix
on the branch you are already on, a repo whose branching you manage yourself.

It is a mode rather than a combination of the other two because every other option asks for
exactly the work it refuses to do, so `no_branch` wins over `in_place`, `existing_branch`
and `base_ref` alike. A worktree is a second checkout, which is the thing being declined, so
the form pins **Workspace** to the main checkout and disables it — the control is held
rather than silently contradicted.

The current branch is still *recorded* (`NULL` on a detached HEAD, a state to report rather
than to fix). It is never `branch_is_new`, so the two reuse guarantees carry over unchanged:
the delete-time check reports that branch's whole unpushed history, and deleting the agent
never deletes the branch. With no worktree to remove either, Delete takes nothing off disk.

> **Warned in the UI:** the agent's changes land in the checkout the operator is working in.
> That is the point of the mode, but it is not the default, so it is stated.

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

All of the above describes the loopback default. Binding a non-loopback address
is opt-in, requires a separate durable credential, and is specified in §12.

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
Server→client: `event`, `status`, `permission_request`, `permission_mode_changed`,
`agent_renamed`, `partial`, `clone_progress`, `rate_limit`.

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
blocks), terminal look on top. Both pages carry a turtle favicon — an inline SVG, drawn for
16px rather than shrunk to it, so a tab is identifiable at a glance.

### The tab as a notification surface
An agent in `awaiting_approval` is **blocked on a human**, and that human is usually in their
editor rather than on this page. The browser tab is the only surface that reaches them there,
so it carries the alert: the title counts what is waiting, and the turtle wears an amber
badge — amber being the colour `awaiting_approval` already has in the UI.

It flashes only while the operator is *not* looking: `document.hidden || !document.hasFocus()`,
so a tab that is visible inside an unfocused window still counts as away, which is the case
the whole thing exists for. Watching it, the badge sits still and the title reads
`(1) claude-web` — the page already shows the amber card, and something blinking under
someone's nose is just noise. It clears the moment the last request is answered.

`document.title` is owned by the flasher and pages set their base title through `setTitle`,
or the two would overwrite each other on every tick.

Only `awaiting_approval` qualifies. `failed` looks like it belongs and does not: it is
terminal, nothing is waiting on the human, and a flash with no answering action would simply
never stop.

**Agent cards hold their place.** They are ordered by `created_at`, which never changes: a
card stays where it was first put for as long as it exists, and a new agent is appended
after the rest. Ordering by `last_active_at` sent whichever agent had just done something to
the front, and since a status message arrives per tool call, two working agents reshuffled
the board continuously — a button could move out from under the cursor between aiming and
clicking.

The list is also reconciled rather than rebuilt. Each card carries a signature of everything
it displays, in display form (`fmtAgo(last_active_at)`, not the raw timestamp, so a figure
that changes on every event but reads the same is not a change). A card whose signature is
unchanged keeps its own DOM node and with it its hover, focus and any in-flight button
state; only the agent that actually changed is re-rendered.

### Usage panel
The dashboard shows one meter per rate-limit window — session (5 hours), week (7 days), and
the overage-included week where the account reports it — each with its utilization and when
it resets. Amber past 60% and red past 90%. An account that reports no per-window breakdown
still names a governing window, which is shown instead of an empty panel.

**The snapshot is persisted**, one row in `rate_limit`, written through on each
`rate_limit_event` and loaded at startup. Held only in memory it died with the process, so
the panel was blank after every restart until an agent happened to run a turn — which is
precisely backwards: the figure matters most when the account is rate-limited and *nothing
can run*. F13's windows carry absolute reset times, so a stored reading stays meaningful.

Two rules keep a restored reading honest, since it can be hours old:

- `captured_at` travels with it, and a snapshot over a minute old is labelled *"as of 2h
  ago"*. Usage only climbs within a window, so an old reading is a floor rather than a lie —
  but it must not read as live.
- A window whose `resetsAt` has passed has rolled over, and the utilization held for it
  belongs to the window before. That meter is dropped rather than shown wrong; if none
  survive, the panel hides.

### Spawn form
Repo picker (§6) · task name (auto-filled from folder, editable) · **Workspace**
(isolated worktree ｜ main checkout) · **Branch** (create a new one ｜ use an existing one,
with the repo's branches listed ｜ stay on the current one, which pins Workspace to the main
checkout) · branch name preview · base ref · model ·
permission mode · optional first message.
**Advanced:** effort, `--add-dir`, `--max-budget-usd`.

### The socket, and a token that has gone stale
The token is minted per boot, so a page left open across a server restart holds a dead one.
A browser hides the HTTP status of a **rejected upgrade** — a refused token and an
unreachable server both surface as a bare `close` with code 1006 — so the reconnect loop
could not tell "come back later" from "never". It chose to come back, every 16s, silently,
for as long as the tab stayed open.

So a close is classified before it is retried: ask `/api/health`, which *does* report its
status. A 401 is a verdict — reconnecting is abandoned and the stale-token banner goes up,
which is the only thing that actually helps, since the operator has to open the new link.
Anything else, an unreachable server included, is the transient case and backs off as
before. A page with no token at all never opens the socket in the first place.

That fix only reaches pages **loaded after it shipped**: a tab already open keeps running
the JavaScript it loaded, so the operator has to reload or close it. Two things follow.

*Assets always revalidate.* They are compiled into the binary and their URLs carry no
content hash, so `common.js` after an upgrade is a different file at the same address. With
no `Cache-Control` at all a browser may heuristically cache it, which makes "reload to pick
up the fix" a coin toss. They are a few KB over loopback, so every asset is served
`no-cache`.

*The refusal log is throttled.* A refusal is worth logging — it is the only sign that
something on this machine is reaching for the control plane without the token — but a stuck
page repeats one every 16s indefinitely, and two of them bury the log. So it is one line a
minute per path, and the line carries how many it stands for. A flood still reads as a
flood; it just takes one line instead of hundreds. Per **path**, so a genuine refusal
somewhere else is never swallowed by a noisy one, and the table of paths is capped.

### Slash commands
Typing `/` opens an autocomplete fed by the `initialize` command list (F9): name,
description, argument hint. Built-in TUI commands are absent from that list, and the CLI
refuses them if typed anyway (F16).

### Message queueing
The input stays live while `Working`; messages queue natively (F6) and render greyed with
an ✕ to cancel. Interrupt cancels the turn only and reports what remains queued.

---

## 8. Notes

A place to write down a task you are not doing now. The portal is where the decision of what
to spawn next gets made, and until now that decision was made entirely from memory.

A note is **inert text and nothing else**: no repo, no branch, no stored spawn parameters, no
button that turns one into an agent. That boundary is the design. A note carrying spawn
parameters is a spawn request with a second lifecycle to keep in sync — the model goes stale,
the branch it names gets used by something else, and the one thing actually wanted (a sentence
reminding you what to do) ends up buried in a form. Retyping a task into the spawn form costs
seconds and keeps the note a memo.

### Schema

```sql
CREATE TABLE notes (
  id         TEXT PRIMARY KEY,      -- uuid
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
```

Created with `CREATE TABLE IF NOT EXISTS` alongside the rest of the schema, so an existing
database gains it on open with nothing to migrate.

No foreign key: notes are one flat global list, not a property of a repo or of an agent.
Tagging a note with a repo was considered and dropped — a column that is usually NULL is a
filter nobody uses. Hanging notes off an agent is worse: they would inherit the agent's
`ON DELETE CASCADE`, and for a note about *future* work that is exactly backwards. The note
should outlive the agent that prompted it.

### Ordering and lifecycle

Ordered `created_at ASC` — oldest first, so the list reads top to bottom as a queue and a new
note appends to the bottom. A note never moves once written. Ordering by `updated_at` was
rejected: it reshuffles the backlog every time a typo is fixed, and gives no way to demote
anything.

**There is no done state.** Finishing a task deletes its note. A `done` flag needs a toggle, a
visual treatment, and a decision about where checked-off items live, and it ends as a graveyard
nobody prunes — where a list of only-still-true things needs none of that. Bodies are editable
in place, so a half-finished task becomes a note describing the other half.

### Endpoints

```
GET    /api/notes          -> [{ id, body, created_at, updated_at }], created_at ASC
POST   /api/notes          { body } -> the created note
PATCH  /api/notes/{id}     { body } -> the updated note
DELETE /api/notes/{id}
```

Behind the same session token as every other route (§7). Bodies are trimmed of surrounding
whitespace; an empty or whitespace-only body is a 400 on create and on edit alike. A body over
8 KiB is a 400 — far past what a memo needs, and it bounds the payload the dashboard poll
carries. There is no cap on how many notes exist.

Saves are last-write-wins with no version check. Optimistic concurrency — PATCH carrying the
`updated_at` it loaded, 409 on a mismatch — is correct in the abstract and wrong here: this is
a single-user loopback memo list, and a 409 the operator cannot meaningfully resolve is worse
than a duplicated edit they would notice immediately.

### The panel

A collapsible panel on the dashboard, beside the agent list. That is the page the portal opens
on, which is exactly the moment "what next" is a live question; behind a nav link it becomes a
backlog nobody reads. Expanded by default, with the collapsed state persisted in
`localStorage` — this frontend's first use of it. The `sessionStorage` choice in `common.js` is
specifically about not letting the *token* outlive the tab, and that reasoning does not
generalise to a UI preference.

A note renders as plain text with `white-space: pre-wrap`, its first line used as the heading
of the collapsed row (derived at render time, never stored). **Not markdown.** The frontend is
embedded in the binary with no Node and no bundler; rendering markdown means vendoring a parser
and then owning a sanitiser, for text only its author will ever read.

Clicking a note opens an in-place editor. **New note** appends an empty row already in that
same editor, held client-side until first save — Escape on an empty row discards it and the
database never sees a blank. One editor, one code path, no separate composer.

Deleting prompts through a native `confirm()`, matching the agent-delete pattern: the body is
text the operator typed and cannot get back.

### Freshness

Notes ride the dashboard's existing poll rather than the socket, so a second tab — or a phone
over Tailscale — catches up within one interval. A wide `is_broadcast_wide` message would be
real protocol work in service of a list of tens of rows.

The poll must not overwrite a note being typed into. A row in edit mode is exempt from
re-render until it is saved or cancelled; every other row updates normally.

### Who can write one

There is no agent-facing tool for notes, and that is a scope decision rather than a boundary.
Agents run as the same user and can reach every token-protected endpoint (§7), `/api/notes`
included, so an agent that goes looking can write one. Enforcing otherwise would mean a second
auth tier for a memo list, which is out of all proportion. The honest position is the one §7
already takes: the token raises the bar, it does not build a wall.

---

## 9. Remote Control

**Not integrated.** The portal already does what Remote Control does — messages, streaming
output, approvals, interrupt — and keeps arbitrary per-agent directories, which Remote
Control cannot (it is scoped to one directory). Phone access comes from §12: binding a
tailnet or private address, behind a paired device key.

Deliberately avoided: an undocumented `-p` + `--remote-control` combination, a subscription
coupling, and two competing permission handlers (our stdio handler vs the phone).

A later escape hatch — a button that launches a *separate* `claude remote-control` server in
a chosen directory, with its own lifecycle and honestly labelled as not-our-agents — remains
possible. The design does not depend on it.

---

## 10. Configuration (`config.toml`)

```toml
port            = 7717
bind            = "127.0.0.1"    # loopback by default; see §12
hostnames       = []             # extra names the Host header may carry (§12)
open_browser    = true
repo_roots      = ["~/Code"]
branch_prefix   = "sw_"
max_agents      = 8
default_model   = "opus"
default_permission_mode = "ask"
claude_bin      = "claude"
pinned_cli_version = "2.1.241"   # warn on mismatch
```

### The two defaults reach the spawn form

`default_model` and `default_permission_mode` are the values a spawn request inherits when it
omits them, but a form that always sends a value never omits anything — so a default that
lives only on the server is a default the UI silently overrides. The spawn panel therefore
reads both from the config on load, and again whenever Settings is saved: the permission
picker opens on the configured mode, and the model field carries it as its placeholder. The
markup's own first option is no longer what a fresh form means.

A config naming a mode this build does not offer leaves the picker alone rather than blanking
it, and a test asserts every `PermissionMode` variant appears in both pickers — a mode the
form cannot select is a default that cannot be honoured.

---

## 11. Out of scope (v1)

Multi-user auth · auto-commit, auto-push, PR creation · Remote Control integration ·
reading Claude's internal transcript files (F11) · agents surviving server death ·
virtualised scrollback · TLS termination in-process (§12 uses a VPN instead).

---

## 12. Remote access

Everything in §7 describes a portal bound to `127.0.0.1`, and that stays the
default: run `claude-web` with no configuration and nothing here applies. This
section covers the opt-in case — binding an address other than loopback so the
portal can be driven from a phone or another machine — and the authentication
that binding requires.

The principal is one person on several devices. There are no accounts, no
per-agent ownership and no authorization layer: a device is either paired or it
is nothing. Multi-user auth remains out of scope (§10).

### What this closes, and what it does not

It closes the network edge. Nothing else.

The residual §7 names — an agent running as the same user can go looking for
whatever credential the browser holds — is unchanged and unimproved here. It is
not made worse either, which is why the loopback path below keeps its own
never-on-disk token rather than being folded into the remote one. Fixing that
residual means putting the control plane somewhere the agents are not (a
separate uid, a socket with peer credentials), and that is a different project.

The thing to be careful about is not letting a local-only residual become a
remote one. That is the reason the per-boot token is refused off-box, and the
reason the durable key is stored hashed.

### Transport: a VPN, not TLS

The binary terminates no TLS and ships no certificate machinery. Encryption and
device authentication come from WireGuard — in practice Tailscale — and the
credential below sits behind that.

A self-signed certificate was the alternative and is worse than nothing: it
cannot authenticate the server, so its only durable effect is teaching you to
click through a browser warning, which is exactly the reflex that makes an
interception attack work. Fronting the server with someone else's reverse proxy
moves the whole security argument into a config file this project neither ships
nor validates.

So the deployment shape is enforced rather than documented. `Config::validate`
refuses any bind address outside:

- loopback,
- RFC1918 private ranges (`10/8`, `172.16/12`, `192.168/16`),
- the carrier-grade NAT range `100.64.0.0/10`, which is where tailnet addresses
  live.

A public bind is not a warning. It does not start.

### Two credentials

Loopback and remote authenticate differently, and each is refused where it does
not belong.

**Loopback — the per-boot token of §7, unchanged.** Minted at startup, never
written to disk, handed to the browser through the URL it is opened with, held
in `sessionStorage`. It is now additionally **refused when the peer address is
not loopback.** It is delivered by strictly local means and therefore never
legitimately arrives from off-box; refusing it there costs one comparison and
keeps §7's one acknowledged leak — a server run as `claude-web > log` writing
the token into that log — a local problem rather than a remotely replayable
credential.

**Remote — a durable key.** 256 bits from the OS random source, generated by
`claude-web pair` (below). It is accepted from any allowed peer, loopback
included: the browser on this machine may perfectly well be a paired device.

The key is presented exactly as the per-boot token is — the `x-claude-web-token`
header, or the `token` query parameter on the `/ws` upgrade, which is the one
place a browser cannot set a header. Same middleware, same seam, one added
branch. It reuses `SessionToken`'s constant-time comparison and its redacted
`Debug`.

**No cookies, and therefore no CSRF.** A session cookie plus a `sessions` table
would buy per-device revocation, at the price of introducing the first identity
state into the schema and reopening a vulnerability class the header-only token
is structurally immune to — on a control plane whose endpoints include
`permission_mode: bypass`. For a handful of devices belonging to one person,
"rotate the key and re-pair" is an adequate revocation story and a much smaller
design.

The consequence is that a paired device holds the key in `localStorage`, not
`sessionStorage`. This is a deliberate departure from §7's reasoning and it is
narrow: a phone that must be re-paired every time the browser drops the tab is
not usable, and the device is one you chose to pair.

### The key on disk

`~/.claude-web/remote-key`, mode 0600, containing **the SHA-256 of the key and
not the key**.

256 bits of entropy is not brute-forceable, so a bare hash verifies it — no
salt, no KDF, nothing that would matter. What this buys is that the file is not
a working credential: an agent that reads it has read nothing it can use. That
is the property §7's never-on-disk token has and that a raw key would have given
away.

It does not live in `config.toml`. The Settings endpoint rewrites that file
wholesale through `Config::save`, so a secret there is one serialisation change
away from being silently dropped, and it would sit in a file the frontend
round-trips.

The cost of hashing is that the raw key exists only in the moment it is
generated. There is no "add a device" — only "re-pair every device". For two or
three devices that is a minute of scanning, and it is the right trade.

### Pairing

`claude-web pair` generates a key, writes its hash, and prints a QR code —
Unicode half-blocks — with the URL underneath for terminals that are too narrow
or whose font mangles the blocks.

**The key rides in the URL fragment**, `#k=<key>`, not the query string. A
fragment is never sent to the server and never appears in a `Referer` header;
the page reads it on load and immediately clears it with `history.replaceState`,
so it does not sit in browser history either. This mirrors the existing
read-from-URL-then-stash handling of `?t=` in `common.js`.

The host part of that URL is the first configured `hostname` if any, otherwise
the bind address. If a hostname is configured at all, that is the way in.

**`pair` refuses to run without a terminal attached**, exiting non-zero. Its
entire output is a secret. The startup path in §7 writes a 0600 file when stdout
is not a terminal because the server has to start however it was invoked; `pair`
is a deliberate interactive act with no headless case worth serving, and the
file variant would only create another on-disk copy of a raw key this design
takes some trouble to avoid keeping.

### Rotation, unpairing, and a device that has gone stale

Running `pair` again generates a new key and overwrites the hash. The old key is
dead immediately, on every device.

`claude-web unpair` deletes the file. Because a non-loopback bind refuses to
start without one, that is also the switch that turns remote access off. It is
the lost-phone procedure: one command.

A device presenting a dead key reaches the same refusal banner as a stale
loopback tab, and that banner's current text — *"Open the link claude-web
printed when it started"* — is useless advice on a phone that has never been
near the terminal. The message therefore branches on the peer address the server
already knows: a non-loopback peer is told to re-pair and scan a fresh code.

### Picking up a new pairing without a restart

The server holds the hash in memory. Requiring a restart to pick up a new one
would be a poor trade, because §10 puts agents surviving server death out of
scope: restarting to pair a tablet would end every agent mid-task.

So when a presented key fails against the cached hash, the server re-reads the
file once and retries — **guarded to at most one re-read per second**, so a
wrong key cannot be turned into a file-read amplifier. A fresh pairing takes
effect on the paired device's first request. No watcher, no signal handler, no
per-request file read.

### Binding and configuration

Two keys join `config.toml`:

```toml
bind      = "127.0.0.1"   # default: loopback
hostnames = []            # extra names the Host header may carry
```

`Config::validate` refuses to start when:

- `bind` is outside loopback / RFC1918 / `100.64.0.0/10`, or
- `bind` is non-loopback and no key file exists.

Both errors name the config key or command that fixes them.

**Neither key is editable through the Settings panel**, which otherwise
round-trips `Config` through the web UI. They are shown read-only if shown at
all. A control-plane client that can widen its own listening address is a
privilege escalation with extra steps, and it is precisely the move a client
that had got hold of a credential would make. Changing where this thing listens
requires touching the machine.

### Host and Origin, off loopback

§7's rebinding defence requires a loopback `Host`, which a tailnet request fails
outright. Rebinding matters more remotely, not less, so the check widens rather
than lifting: the allowlist becomes loopback, the configured bind address, and
each configured hostname — on the port actually being served.

A hostile domain rebound to your tailnet address still arrives with
`Host: evil.example`, which is on no list. `Origin` and `Sec-Fetch-Site` are
unchanged; same-origin is same-origin wherever the page was served from.

Loopback keeps its own listener whatever `bind` says, so the local browser is
still opened on the loopback tokened URL and the per-boot token still has
somewhere it is legitimately presented. `bind` therefore *adds* a listener
rather than replacing one — including for `::1`, which would otherwise be a
value the config accepts and the server silently ignores.

### What a paired device may do

Everything a loopback client may do, including relaxing an agent's permission
mode.

Restricting that one endpoint looks prudent and is close to theatre: a client
that can approve each individual tool call already reaches everywhere bypass
mode reaches, one prompt at a time. And approvals are the reason to want this on
a phone at all — a portal that shows a blocked agent but makes you walk to a
desk to unblock it has failed at its only job. A credential not trusted enough
for the full verb set is a device that should not have been paired.

Multiple attached clients need no new machinery. `/ws` already fans out from
`sup.subscribe()`, and `decide` resolves a permission request by removing it
from the pending map, so the first answer wins and the second is told the
approval is no longer outstanding.

### Attribution

§7 already writes a permission-mode change into the agent's event log with its
initiator. With two credential paths, that field gains a meaning it did not have
on loopback: *which client*.

Permission decisions and permission-mode changes therefore record the channel —
`local` or `paired` — and, for paired requests, the peer address. The field is
the existing one; nothing new is added to the schema.

Having given up per-device sessions, this log is the only visibility into which
devices are acting. That is what it is for.

### Failed authentication

No lockout, no throttling. The key is not guessable, so a limiter would protect
nothing and would add a state machine to maintain. (`rate_limit` in the schema
is a cache of Claude's usage headers and is unrelated.)

The existing refusal logging stands: a warning per refusal, escalating on
repeats. Refusals from a non-loopback peer additionally record that peer's
address, because someone on your tailnet failing to authenticate is worth more
than a local page whose token went stale across a restart. The presented
credential is never logged, valid or not.

### Startup

A non-loopback bind prints, plainly, the address it is listening on and that the
portal is reachable from the network.

The local browser is still opened on the loopback tokened URL. The durable key
is never printed at startup and never reaches an `open` command line, for the
same reason the per-boot token does not: an argv is readable by every process on
the machine.

### Tests

The boundary properties are asserted directly, in the enumerating style of
`every_api_route_needs_the_token` — a route added later that forgets the check
is the failure this catches:

1. With neither credential, every `/api/*` route and the `/ws` upgrade is
   refused.
2. The per-boot token is refused when the peer address is not loopback.
3. A non-loopback bind refuses to start when no key file exists.
4. A public-IP bind is refused by `Config::validate`.
5. `Host` is accepted for loopback, the configured bind address and each
   configured hostname, and refused for anything else.
6. The key file never contains the raw key.

### Dependencies

One addition: a pure-Rust QR encoder for `pair`. The alternative — printing the
URL and letting you type 64 hex characters into a phone — is bad enough that it
would be routed around by pasting the key into a chat app, which is a worse
outcome than the dependency.

---

## 13. Note before implementation

The project directory is `~/Code/claude web` — **the space breaks `cargo init`'s default
package name.** Use `cargo init --name claude-web`.
