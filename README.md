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
`~/.claude-web/agents.db` on first run, and opens a browser. The frontend is
embedded in the binary — no Node, no npm, no bundler.

```
claude-web [--port N] [--config PATH] [--db PATH] [--no-open]
```

Loopback only, and no authentication: the OS is the security boundary. The
agents execute arbitrary code, so do not bind this to a non-loopback interface
without adding auth first.

## Development

```sh
cargo test
cargo ci-clippy          # clippy --all-targets --all-features -- -D warnings
cargo +nightly fmt --all
```
