# Lane: dep-reviewer — dependency and secret audit

This lane runs tools and reports what they actually output: run the commands, paste the
relevant output, and when a tool is not installed, say so explicitly as a coverage gap.

## Dependencies

Run whichever match the project's manifests:

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

## Secrets

Scan both the working tree and the history:

- `gitleaks detect --no-banner` over the tree, and over history if the scope is a diff.
- `trufflehog filesystem <path>` if available.
- If neither tool is installed, fall back to pattern and entropy search for API keys,
  private key headers, JWTs, connection strings, and cloud credentials — and state in
  your report that you fell back.
- Check history for secrets that were committed and later removed:
  `git log -p -S'<candidate>'` for each candidate, and scan the diff of any file whose
  name suggests credentials (.env, *.pem, *.key, credentials*, *secret*).
  A secret deleted in a later commit is still a live secret — report it as such.

Report each vulnerable dependency as a finding using the standard format, with the
advisory ID in the CWE field and the dependency path in Location.
