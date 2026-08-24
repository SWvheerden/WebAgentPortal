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
| F12 | Remote Control (`claude remote-control`) is a persistent server, ≤32 concurrent sessions, outbound-only, but **scoped to one directory** and requiring a full-scope subscription login. Compatibility with `-p` is **undocumented and untested** (testing would create a real session on the account). | `claude remote-control --help` + docs. |

### Risk register

- **`--permission-prompt-tool` is a hidden flag** and the stream-json protocol carries no
  stability guarantee. Mitigation: pin a known-good CLI version in config, assert on
  `system/init` at startup, log-and-surface unrecognised event types rather than crashing,
  and keep every raw event in the DB so nothing is lost to a parser gap.
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
  base_ref          TEXT,
  uses_worktree     INTEGER NOT NULL,
  permission_mode   TEXT NOT NULL,         -- ask | acceptEdits | bypass | dangerous
  model             TEXT,
  effort            TEXT,
  max_budget_usd    REAL,
  status            TEXT NOT NULL,
  status_detail     TEXT,                  -- e.g. "Bash: cargo test"
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
pid,ppid,pgid`, walked breadth-first with a visited set, in `spawn_blocking`)
shortly after each tool call starts and again when it returns or the turn ends;
the distinct process groups found are accumulated over the session, bounded, and
never replaced — a tool call that has already returned can still have left
something running.

Teardown then runs on **every** exit path, not only an operator Stop: a crash, a
budget exhaustion, a non-zero exit. It SIGTERMs each recorded group that still
holds a process from the snapshot, waits the 5s grace, and SIGKILLs whatever is
left. It is awaited inside the runner, so the agent is not deregistered — and
the server is not allowed to finish shutting down — until it has completed.
Without that, shutdown returned as soon as the CLI died and the escalation was
cancelled with the runtime. pid 1, the server's own pid and the server's own
process group are never signalled, and a group is only signalled while it still
holds a process we recorded, so a recycled group id is not caught in the blast.

**Residual limitation, not fixed and not fixable here.** A process the agent
*detaches* — `nohup … &`, `setsid`, anything reparented to pid 1 before a
snapshot sees it — is in neither the CLI's process group nor its subtree at any
point we look. Nothing connects it back to the agent, and macOS has no cgroup or
PID-namespace equivalent to catch it, so it survives both Stop and server
shutdown. Confirmed live: a `nohup sleep 941 &` started by a tool call outlives
both. The same applies to anything that starts and detaches entirely between two
snapshots, and to a server killed with SIGKILL, which runs no teardown at all.
When a worktree removal is refused because something is still holding it, git's
own stderr is surfaced to the operator with the path and a "delete anyway" hint
rather than a generic failure.

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
A **"work in the main checkout instead"** toggle opts into in-place `checkout -b`.

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

Loopback only (`127.0.0.1:7717`), no auth for now; the OS is the security boundary.
Auth arrives when it binds to a non-loopback interface — mandatory, given the agents
execute arbitrary code.

### Host and Origin

Loopback binding alone does not survive DNS rebinding: a page served from
`http://evil.example:7717` and rebound to `127.0.0.1` is same-origin as far as
the browser is concerned. Every request is therefore checked for a loopback
`Host` header on the port actually being served, and any request carrying a
foreign `Origin` — WebSocket upgrades included — is refused with 403.

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
GET  /api/config, PUT /api/config
WS   /ws                        multiplexed, {agent_id, ...}-tagged envelope
```

One WebSocket serves both dashboard and detail views: one reconnect path, one schema.
Client→server: `send_message`, `permission_decision`, `interrupt`, `subscribe{agent_id, after_seq}`.
Server→client: `event`, `status`, `permission_request`, `partial`, `clone_progress`.

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
