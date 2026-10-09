# security-analysis requirements

The preflight checks every row that applies to the scope. A skill is present when it appears
in your available-skills list; a tool is present when `command -v <tool>` (or the check
shown) succeeds.

## Skills

| Requirement | Used by | Install | If the user continues without it |
|---|---|---|---|
| `cocoindex-code:ccc` skill and `ccc` CLI | Step 0e, every lane | user runs `/plugin install cocoindex-code@cocoindex-code` | lanes navigate with grep and direct reads |
| `senior-security` skill | `secskill-reviewer` | copy `engineering-team/skills/senior-security` from https://github.com/alirezarezvani/claude-skills into `~/.claude/skills/` (see below) | the lane does the STRIDE/DREAD pass directly from its brief |
| `red-team` skill | `attack-path-reviewer` | copy `engineering-team/skills/red-team` from https://github.com/alirezarezvani/claude-skills into `~/.claude/skills/` (see below) | the lane chains findings directly from its brief |
| `/security-review` command (diff scope only) | `security-reviewer` | built into Claude Code — user updates Claude Code | the lane reviews the in-scope files directly |

To install either skill, run (with `<skill>` set to `senior-security` or `red-team`):

```
git clone --depth 1 https://github.com/alirezarezvani/claude-skills "$TMPDIR/claude-skills"
cp -R "$TMPDIR/claude-skills/engineering-team/skills/<skill>" ~/.claude/skills/
```

## Tools — check only the rows matching the project's manifests

| Tool | Applies when | Install |
|---|---|---|
| `cargo-audit` | `Cargo.lock` | `cargo install cargo-audit --locked` |
| `cargo-deny` | `deny.toml` | `cargo install cargo-deny --locked` |
| `npm` / `pnpm` / `yarn` | the matching JS lockfile | ships with Node / `corepack enable` |
| `pip-audit` | Python manifests | `pipx install pip-audit` |
| `govulncheck` | `go.mod` | `go install golang.org/x/vuln/cmd/govulncheck@latest` |
| `bundle-audit` | `Gemfile.lock` | `gem install bundler-audit` |
| `gitleaks` | always | `brew install gitleaks` |
| `trufflehog` | always | `brew install trufflehog` |

A tool the user continues without is skipped by `dep-reviewer`, which falls back as its
brief describes.

## Asking

Collect every missing requirement, then ask in one `AskUserQuestion` call — one question
per missing requirement, grouping related tools into one question when more than four are
missing. Options:

- **Install now (Recommended)** — when the install is a shell command (every tool, and the
  `senior-security` / `red-team` skills), run it yourself, then re-check. For a plugin or
  built-in command, give the user the step to run, wait for them to confirm, then re-check
  your skills list.
- **Continue without it** — apply the fallback, and list it under coverage gaps in the
  final report.
- **Stop** — end the run before spawning anything.

If an install fails or the re-check still finds it missing, say so and ask again with the
same options. The preflight is **done** when every requirement is present or the user has
chosen to continue without it.
