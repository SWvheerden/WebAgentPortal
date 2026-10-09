# Lane: gen-reviewer — application security

Think like a red-team attacker for every piece of code in scope.

Apply the **All profiles** section, plus each section named in the preamble's `Profile`
line. Skip the sections for profiles that are not listed.

## All profiles

- Input validation: is every externally-controlled input validated before use, at the
  boundary, against an allow-list rather than a deny-list?
- Injection: is input ever interpolated into SQL, shell commands, HTML, file paths,
  format strings, log lines, or regexes?
- Secrets: any credentials, keys, or tokens hardcoded? Any new env var undocumented?
- Error handling: do errors leak stack traces, internal paths, or query text to callers?
- Logging: is sensitive data (tokens, keys, PII, full request bodies) written to logs?

## web-service

- IDOR: can User A reach User B's data by changing an ID? Check every route with an id param.
- Auth: is every new endpoint behind auth middleware? Are permissions checked, not just
  authentication? Is the check server-side and not inferable from the client?
- Session management, CSRF protection, cookie flags (HttpOnly, Secure, SameSite).
- XSS via unescaped input in templates or client-rendered HTML.
- Data exposure: do responses leak internal fields — password hashes, internal IDs,
  other users' records, soft-deleted rows?
- Configuration: debug mode, permissive CORS, missing security headers, verbose errors.
- Rate limiting on auth, password reset, and expensive endpoints.

## library-ffi

- Every `unsafe` block: is there a SAFETY comment, and is the invariant actually upheld?
- FFI boundary: memory allocated by C is freed by C; Rust memory handed to foreign code has
  the correct layout; no dangling pointers across the boundary.
- Raw pointer arithmetic, transmute, and casts — check for UB and provenance violations.
- Panic across an FFI boundary is UB: is unwinding caught at the boundary?
- Public API surface: can a caller trigger UB using only safe API? That is a soundness bug.
- Input from untrusted callers: lengths, null-termination, alignment, aliasing.

## cli-tooling

- Shell-out: is any argument attacker-influenced? Expect argv arrays over shell strings.
- Path traversal in file arguments; symlink following; TOCTOU between check and open.
- Temp file creation: predictable names, missing O_EXCL, world-readable permissions.
- Environment and config file trust: does the tool execute or load anything it finds?
- Credentials passed as command-line arguments (visible in ps) or written to shell history.

## node-consensus

- The `crypto-reviewer` lane covers this profile. Report only what its brief (crypto,
  arithmetic, untrusted-input DoS, protocol and consensus) leaves out.

Report back all findings.
