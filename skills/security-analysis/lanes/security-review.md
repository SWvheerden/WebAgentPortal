# Lane: security-reviewer

Do a security review of the code in scope.

- Scope type `diff`: run `/security-review` — it reviews the pending changes on the current
  branch, which is exactly this scope.
- Scope type `diff` with `/security-review` listed as unavailable: review the changed
  files directly.
- Scope type `path`: review the in-scope files directly. `/security-review` only inspects
  pending branch changes, so for a path scope it would see nothing.

Report back all findings.
