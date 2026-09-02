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
When stdout is not a terminal the link is written to
`~/.claude-web/session-url.html` (mode 0600) instead of being printed, so
redirecting the server's output to a log file does not put the token in it.

```
claude-web [--port N] [--config PATH] [--db PATH] [--no-open]
claude-web pair          # pair a device for remote access
claude-web unpair        # forget it again
```

Loopback by default. The session token above raises the bar in front of the
control plane — including against the agents themselves, which can otherwise
reach every endpoint that constrains them — but it cannot be hidden from a
determined process running as the same user; see DESIGN §7 for what it does and
does not achieve.

## Reaching it from a phone

Set `bind` in `config.toml` to a private or tailnet address and run
`claude-web pair`, which prints a QR code to scan. The server terminates no TLS,
so it refuses to start on anything but loopback, RFC1918 or `100.64.0.0/10`:
encryption and device authentication come from the VPN (Tailscale, in practice),
and the paired key sits behind it. Loopback keeps its own listener either way.

Only the hash of that key is stored, in `~/.claude-web/remote-key` (mode 0600),
so the file is not itself a working credential. Pairing again replaces it
everywhere; `claude-web unpair` deletes it, which also stops a non-loopback bind
from starting — that is the lost-phone procedure. See DESIGN §12.

## Development

```sh
cargo test
cargo ci-clippy          # clippy --all-targets --all-features -- -D warnings
cargo +nightly fmt --all
```
