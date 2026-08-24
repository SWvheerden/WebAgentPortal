# claude-web

A local Rust web server that spawns and supervises multiple long-lived Claude Code
agents. Each agent owns a task, a name, a git branch and an isolated worktree.
A browser portal lists every agent with live status and lets you drive any one of
them like a terminal session.

The full design is in [DESIGN.md](DESIGN.md).

## Running

```sh
cargo run --release
```

It binds `127.0.0.1:7717`, writes `~/.claude-web/config.toml` and
`~/.claude-web/agents.db` on first run, and opens a browser at the URL it
prints. The frontend is embedded in the binary — no Node, no npm, no bundler.

That URL carries a session token minted at startup: every API call and the
WebSocket require it, and it changes on every restart. Open the printed link (or
paste it) rather than typing the bare address, or the page will have no token.

```
claude-web [--port N] [--config PATH] [--db PATH] [--no-open]
```

Loopback only. The session token above is what keeps the control plane out of
reach of other local processes — including the agents themselves, which can
otherwise reach every endpoint that constrains them. Do not bind this to a
non-loopback interface: it would need real multi-user authentication first.

## Development

```sh
cargo test
cargo ci-clippy          # clippy --all-targets --all-features -- -D warnings
cargo +nightly fmt --all
```
