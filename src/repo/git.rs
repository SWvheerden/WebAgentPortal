//! Branch naming, worktree management and the delete-time safety checks.
//!
//! Everything in this module shells out to `git` and blocks; async callers must
//! wrap these in `tokio::task::spawn_blocking`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Longest slug we generate before collision suffixing.
pub const MAX_SLUG_LEN: usize = 40;

/// Turn a task name into a URL/branch-safe slug: lowercase, non-alphanumerics
/// collapsed to `_`, trimmed, capped at [`MAX_SLUG_LEN`] characters.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if ch.is_alphanumeric() {
            // Non-ASCII letters and digits are not branch-safe everywhere.
            if !last_underscore {
                out.push('_');
                last_underscore = true;
            }
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
        if out.chars().count() >= MAX_SLUG_LEN {
            break;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "agent".to_string()
    } else {
        trimmed
    }
}

/// `<prefix><slug>`, e.g. `sw_fix_the_parser`.
pub fn branch_name(prefix: &str, slug: &str) -> String {
    format!("{prefix}{slug}")
}

/// Append `_2`, `_3`, … until `is_taken` says the name is free.
pub fn unique_name(base: &str, is_taken: impl Fn(&str) -> bool) -> String {
    if !is_taken(base) {
        return base.to_string();
    }
    for n in 2..10_000 {
        let candidate = format!("{base}_{n}");
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    format!("{base}_{}", uuid::Uuid::new_v4().simple())
}

/// Pick a free slug and the matching branch name in one go, so the two never
/// drift apart: the suffix that makes the slug unique also lands on the branch.
pub fn allocate_names(
    task_name: &str,
    prefix: &str,
    taken_slugs: &HashSet<String>,
    taken_branches: &HashSet<String>,
) -> (String, String) {
    let base = slugify(task_name);
    let slug = unique_name(&base, |candidate| {
        taken_slugs.contains(candidate) || taken_branches.contains(&branch_name(prefix, candidate))
    });
    let branch = branch_name(prefix, &slug);
    (slug, branch)
}

/// Where an agent's worktree lives: `<root>/.worktrees/<repo>/<slug>`, outside
/// the repo and under a dot-directory the scanner already skips.
pub fn worktree_path(root: &Path, repo_name: &str, slug: &str) -> PathBuf {
    root.join(".worktrees").join(repo_name).join(slug)
}

/// Reject anything we would hand to git as a positional argument that git
/// could mistake for an option, or that is not a valid ref.
///
/// git accepts options *after* positionals, so a `base_ref` of
/// `--output=/tmp/x` would otherwise become `git log --output=…` — an arbitrary
/// file write. Every user-supplied ref goes through here before it is used, and
/// is then resolved to an object id so only hex reaches the command line.
pub fn validate_ref(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("ref cannot be empty");
    }
    if name.starts_with('-') {
        bail!("`{name}` starts with `-`, which git would read as an option");
    }
    if name.starts_with('/') || name.ends_with('/') || name.ends_with('.') {
        bail!("`{name}` is not a valid ref name");
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") || name.ends_with(".lock")
    {
        bail!("`{name}` is not a valid ref name");
    }
    for ch in name.chars() {
        if ch.is_control() || ch.is_whitespace() || !ch.is_ascii() {
            bail!("`{name}` contains a character git does not allow in a ref name");
        }
        if matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\u{7f}') {
            bail!("`{name}` contains `{ch}`, which git does not allow in a ref name");
        }
    }
    Ok(())
}

/// A branch prefix is a ref fragment: the same rules, except it may be empty
/// and may end in a separator.
pub fn validate_branch_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.starts_with('-') {
        bail!("branch prefix `{prefix}` starts with `-`, which git would read as an option");
    }
    if prefix.starts_with('/') || prefix.contains("..") || prefix.contains("@{") {
        bail!("branch prefix `{prefix}` is not a valid ref fragment");
    }
    for ch in prefix.chars() {
        if ch.is_control() || ch.is_whitespace() || !ch.is_ascii() {
            bail!("branch prefix `{prefix}` contains a character git does not allow");
        }
        if matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\u{7f}') {
            bail!("branch prefix `{prefix}` contains `{ch}`, which git does not allow");
        }
    }
    Ok(())
}

/// A path we pass to git must not look like an option either.
fn checked_path(path: &Path) -> Result<&str> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))?;
    if text.starts_with('-') {
        bail!("`{text}` starts with `-`, which git would read as an option");
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// git plumbing
// ---------------------------------------------------------------------------

/// Config that must not come from the repository we are inspecting.
///
/// Several git config keys are command strings, and git honours the *target*
/// directory's `.git/config`. A directory the user merely unzipped can
/// therefore run arbitrary commands the moment the repo picker scans it or an
/// endpoint asks for its branches. Every invocation overrides those keys with
/// `-c`, which takes precedence over any config file.
pub const SAFE_CONFIG: &[&str] = &[
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.sshCommand=",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.pager=cat",
    "-c",
    "core.editor=true",
    "-c",
    "core.askPass=",
    "-c",
    "credential.helper=",
    "-c",
    "diff.external=",
    "-c",
    "uploadpack.packObjectsHook=",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "protocol.allow=user",
    "-c",
    "core.gitProxy=",
    "-c",
    "core.alternateRefsCommand=",
    "-c",
    "sequence.editor=true",
    "--no-optional-locks",
];

/// The last component of a config key, lowercased.
fn key_leaf(key: &str) -> &str {
    key.rsplit('.').next().unwrap_or(key)
}

/// Config keys whose values git runs, and the value that provably disarms
/// each. `*` matches one component, so `filter.*.clean` covers a driver of any
/// name — which a fixed `-c` list cannot, because the name is the attacker's to
/// choose.
///
/// The disarming value is per key. Both `core.hooksPath=` and
/// `core.hooksPath=/dev/null` suppress hooks on git 2.52 (measured against a
/// control that ran the hook); `/dev/null` is kept because it says what it
/// means. `remote.*.uploadpack` and `remote.*.receivepack` are deliberately
/// absent — see [`FLAG_PROTECTED_KEYS`].
const EXECUTED_KEYS: &[(&str, &str)] = &[
    ("core.fsmonitor", ""),
    ("core.sshcommand", ""),
    ("core.pager", "cat"),
    ("core.editor", "true"),
    ("core.askpass", ""),
    ("core.gitproxy", ""),
    ("core.alternaterefscommand", ""),
    ("core.hookspath", "/dev/null"),
    ("credential.helper", ""),
    ("credential.*.helper", ""),
    ("diff.external", ""),
    ("diff.*.command", ""),
    ("diff.*.textconv", ""),
    ("merge.*.driver", ""),
    ("mergetool.*.cmd", ""),
    ("mergetool.*.path", ""),
    ("difftool.*.cmd", ""),
    ("difftool.*.path", ""),
    ("filter.*.clean", ""),
    ("filter.*.smudge", ""),
    ("filter.*.process", ""),
    ("uploadpack.packobjectshook", ""),
    ("sequence.editor", "true"),
    ("gpg.program", "true"),
    ("gpg.*.program", "true"),
    ("browser.*.cmd", ""),
    ("guitool.*.cmd", ""),
    ("trailer.*.command", ""),
    ("submodule.*.update", ""),
    ("init.templatedir", ""),
    ("imap.tunnel", ""),
    ("pager.*", "cat"),
    ("alias.*", ""),
];

/// Keys `-c` cannot disarm, which a command-line argument handles instead.
///
/// Measured on git 2.52 with a control: with the repository declaring
/// `remote.origin.uploadpack`, the command ran under no override, and still ran
/// under `-c remote.origin.uploadpack=` and `-c …=git-upload-pack` — for
/// `fetch --all`, `fetch origin` and `ls-remote`, over both a bare path and a
/// `file://` URL — even though `config --get` showed the override had landed.
/// Only `--upload-pack=git-upload-pack` suppressed it.
///
/// So these are neither disarmed nor refused: [`fetch`] pins the argument, and
/// `fetch` is the only command here that contacts a remote at all. Refusing
/// them instead would make a repository with a legitimate custom upload-pack —
/// which some corporate setups need — unusable.
const FLAG_PROTECTED_KEYS: &[&str] = &["remote.*.uploadpack", "remote.*.receivepack"];

/// Words that end the name of a key git is likely to execute.
///
/// This is the catch-all half. Two rounds of this were an allowlist of specific
/// dangerous keys and both were incomplete — `core.fsmonitor` was covered while
/// `filter.*.clean` was not — so anything matching this that is *not* a shape
/// we know how to disarm makes the repository refuse inspection outright,
/// rather than being run in the hope that it is harmless.
const COMMAND_LEAVES: &[&str] = &[
    "askpass",
    "clean",
    "cmd",
    "command",
    "driver",
    "editor",
    "external",
    "fsmonitor",
    "helper",
    "hook",
    "hookspath",
    "pager",
    "process",
    "program",
    "run",
    "script",
    "smudge",
    "sshcommand",
    "templatedir",
    "textconv",
    "tunnel",
    "uploadpack",
    "receivepack",
    "vcs",
];

fn matches_shape(key: &str, shape: &str) -> bool {
    let key: Vec<&str> = key.split('.').collect();
    let shape: Vec<&str> = shape.split('.').collect();
    if key.len() != shape.len() {
        return false;
    }
    key.iter()
        .zip(shape.iter())
        .all(|(k, s)| *s == "*" || s == k)
}

/// Does git execute this config value, as far as we can tell?
fn is_command_valued(key: &str, value: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let value = value.trim();
    // git's own escape hatches: an alias or submodule update starting with `!`
    // is a shell command, and `ext::` is a transport that runs one.
    if value.starts_with('!') || value.starts_with("ext::") {
        return true;
    }
    if value.is_empty() {
        return false;
    }
    disarm_value(&key).is_some()
        || is_flag_protected(&key)
        || COMMAND_LEAVES.contains(&key_leaf(&key))
}

/// Is this key handled by a command-line argument rather than by `-c`?
fn is_flag_protected(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    FLAG_PROTECTED_KEYS
        .iter()
        .any(|shape| matches_shape(&key, shape))
}

/// The value that disarms this key, if we know one. `None` means we cannot make
/// it provably inert, and the repository is refused rather than run.
fn disarm_value(key: &str) -> Option<&'static str> {
    let key = key.to_ascii_lowercase();
    EXECUTED_KEYS
        .iter()
        .find(|(shape, _)| matches_shape(&key, shape))
        .map(|(_, disarmed)| *disarmed)
}

/// What a repository's own config forces us to do before touching it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoGuard {
    /// `-c key=` pairs disarming every command-valued key the repository
    /// declares, by the exact names it declares them under.
    pub overrides: Vec<String>,
    /// Keys that execute something and that blanking would not disarm. A
    /// repository declaring one of these is not touched at all.
    pub refusals: Vec<String>,
}

impl RepoGuard {
    /// Build the guard from the configuration a repository actually puts into
    /// effect.
    ///
    /// `git config --list` reads config files and nothing else — no working
    /// tree, so no filter driver runs — which is what makes it safe to ask the
    /// repository about itself before working in it.
    ///
    /// Two listings, both with `--includes`, because `--local` alone does not
    /// show the effective configuration:
    ///
    /// * `include.path` pulls keys in from another file. Without `--includes`
    ///   the only trace is the `include.path` entry itself, which is ordinary
    ///   data — so a filter driver hidden behind one was invisible here while
    ///   git honoured it.
    /// * `extensions.worktreeConfig = true` puts keys in `.git/config.worktree`,
    ///   which `--local` never reads.
    ///
    /// Deliberately *not* an unscoped `git config --list`: that would pull in
    /// the operator's global config, where a normal `filter.lfs.clean` or
    /// `credential.helper` would be blanked — breaking git-lfs and
    /// authentication — and an unrecognised global key would refuse every
    /// repository they own.
    pub fn read(repo: &Path) -> Self {
        if !is_git_repo(repo) {
            // Not a repository: there is no repository config to guard.
            return Self::default();
        }
        let local = match git_raw(
            repo,
            &[],
            &["config", "--local", "--list", "--includes", "-z"],
        ) {
            Ok(listing) => listing,
            Err(err) => {
                // A repository whose config git will not read is one we cannot
                // vet. Running it under the fixed list alone is the fail-open
                // case this guard exists to remove.
                return Self {
                    overrides: Vec::new(),
                    refusals: vec![format!("its git config could not be read ({err})")],
                };
            }
        };
        let mut guard = Self::from_listing(&local);
        // Errors when the worktreeConfig extension is off, which correctly
        // means there is no worktree config in effect.
        if let Ok(worktree) = git_raw(
            repo,
            &[],
            &["config", "--worktree", "--list", "--includes", "-z"],
        ) {
            guard.absorb(Self::from_listing(&worktree));
        }
        guard
    }

    /// Fold a second listing's findings in.
    fn absorb(&mut self, other: Self) {
        for override_ in other.overrides {
            if !self.overrides.contains(&override_) {
                self.overrides.push(override_);
            }
        }
        for refusal in other.refusals {
            if !self.refusals.contains(&refusal) {
                self.refusals.push(refusal);
            }
        }
    }

    /// Parse `git config --list -z` output: NUL-separated records, each
    /// `key\nvalue` (a valueless key has no newline).
    pub fn from_listing(listing: &str) -> Self {
        let mut guard = Self::default();
        for record in listing.split('\0').filter(|r| !r.is_empty()) {
            let (key, value) = match record.split_once('\n') {
                Some((key, value)) => (key, value),
                None => (record, ""),
            };
            if !is_command_valued(key, value) {
                continue;
            }
            if is_flag_protected(key) {
                // Handled by the argument `fetch` pins, not by `-c`.
                continue;
            }
            if let Some(inert) = disarm_value(key) {
                let disarmed = format!("{}={inert}", key.to_ascii_lowercase());
                if !guard.overrides.iter().any(|o| o == &disarmed) {
                    guard.overrides.push(disarmed);
                }
            } else {
                let refusal = format!("{key} = {value}");
                if !guard.refusals.contains(&refusal) {
                    guard.refusals.push(refusal);
                }
            }
        }
        guard
    }

    /// The `-c key=` arguments, ready to splice into an argv.
    pub fn args(&self) -> Vec<&str> {
        let mut args = Vec::with_capacity(self.overrides.len() * 2);
        for override_ in &self.overrides {
            args.push("-c");
            args.push(override_.as_str());
        }
        args
    }

    /// Refuse to touch a repository whose config runs something we cannot
    /// disarm. Failing closed is the point: the operator is told which key,
    /// rather than the command being run on their behalf.
    pub fn check(&self, repo: &Path) -> Result<()> {
        if self.refusals.is_empty() {
            return Ok(());
        }
        bail!(
            "{} declares git config that runs commands ({}); claude-web will not inspect or \
             work in it. Remove those keys from its .git/config if you trust the repository.",
            repo.display(),
            self.refusals.join(", ")
        )
    }
}

/// Environment every `git` call runs with: no credential prompts, no askpass
/// helper, no interactive ssh.
pub fn harden(command: &mut Command) -> &mut Command {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .env("GIT_OPTIONAL_LOCKS", "0")
}

/// [`harden`] for the async clone path.
pub fn harden_async(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .env("GIT_OPTIONAL_LOCKS", "0")
}

/// Run `git` in `cwd` with the fixed hardening plus `extra`, returning trimmed
/// stdout.
///
/// The low-level runner: it does not consult the repository's own config, so
/// [`RepoGuard::read`] can use it without recursion.
fn git_raw(cwd: &Path, extra: &[&str], args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(SAFE_CONFIG)
        .args(extra)
        .args(args);
    let out = harden(&mut command)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run `git` in `cwd`, disarming whatever that repository's config declares
/// first — and refusing outright if it declares something we cannot disarm.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let guard = RepoGuard::read(cwd);
    guard.check(cwd)?;
    git_raw(cwd, &guard.args(), args)
}

/// Run `git`, returning `None` instead of an error when it fails.
fn git_opt(cwd: &Path, args: &[&str]) -> Option<String> {
    git(cwd, args).ok()
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Is this directory a git working tree? Cheap enough for a 46-entry scan.
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Current branch name, or `None` on a detached HEAD or a broken repo.
pub fn current_branch(repo: &Path) -> Option<String> {
    let name = git_opt(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name)
    }
}

/// Uncommitted changes present? Untracked files count.
pub fn is_dirty(repo: &Path) -> bool {
    git_opt(repo, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty())
}

/// What the picker needs to know about one repository.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoMeta {
    pub branch: Option<String>,
    pub dirty: bool,
    /// Why the repository was not inspected, when its own config forbade it.
    pub refused: Option<String>,
}

/// Read a repository's branch and dirtiness in one pass.
///
/// The scanner runs this over every directory under every root, so the guard is
/// built once here rather than once per git call — and a repository that
/// refuses inspection is reported as such instead of being run.
pub fn repo_metadata(repo: &Path) -> RepoMeta {
    let guard = RepoGuard::read(repo);
    if let Err(err) = guard.check(repo) {
        return RepoMeta {
            refused: Some(format!("{err}")),
            ..Default::default()
        };
    }
    let extra = guard.args();
    let branch = git_raw(repo, &extra, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|name| !name.is_empty() && name != "HEAD");
    let dirty =
        git_raw(repo, &extra, &["status", "--porcelain"]).is_ok_and(|s| !s.trim().is_empty());
    RepoMeta {
        branch,
        dirty,
        refused: None,
    }
}

/// Local branches, for the base-ref dropdown. Current branch first.
pub fn list_branches(repo: &Path) -> Vec<String> {
    let mut branches = git_opt(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .map(|s| lines(&s))
    .unwrap_or_default();
    if let Some(current) = current_branch(repo) {
        branches.retain(|b| *b != current);
        branches.insert(0, current);
    }
    branches
}

/// `git fetch --all --prune`. Never automatic — it is a button (§6).
///
/// Runs with the same credential-prompt and askpass hardening as cloning, so a
/// remote that wants credentials fails fast instead of hanging or shelling out.
pub fn fetch(repo: &Path) -> Result<String> {
    // `--upload-pack` on the command line is the only thing that overrides a
    // remote's `uploadpack` key — `-c` does not, measured — so it is pinned
    // here as well as being a refusal in the guard.
    git(
        repo,
        &["fetch", "--all", "--prune", "--upload-pack=git-upload-pack"],
    )
}

/// Validate a ref and resolve it to a commit object id.
///
/// Callers pass the resolved id, never the user's text, to any command that
/// takes a positional revision.
pub fn resolve_commit(repo: &Path, reference: &str) -> Result<String> {
    validate_ref(reference)?;
    let oid = git(
        repo,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .with_context(|| format!("resolving `{reference}`"))?;
    if oid.is_empty() {
        bail!("`{reference}` does not name a commit");
    }
    Ok(oid)
}

/// `git worktree add -b <branch> -- <path> [<resolved base>]`.
///
/// The base ref is resolved to an object id first, and `--` closes the option
/// list, so neither can be turned into a flag.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base_ref: Option<&str>) -> Result<()> {
    validate_ref(branch)?;
    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }
    let path_str = checked_path(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let base_oid = base_ref
        .map(|base| resolve_commit(repo, base))
        .transpose()?;
    let mut args = vec!["worktree", "add", "-b", branch, "--", path_str];
    if let Some(oid) = &base_oid {
        args.push(oid);
    }
    git(repo, &args)?;
    Ok(())
}

/// `git worktree remove`, then prune the administrative files.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = checked_path(path)?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push("--");
    args.push(path_str);
    let result = git(repo, &args);
    if let Err(err) = &result
        && force
    {
        // The checkout may already be gone; drop the registration either way,
        // but keep git's own words on why it refused.
        let original = format!("{err:#}");
        // The checkout directory may already be gone — that is not a failure,
        // and it must not stop us pruning the stale registration git is still
        // holding.
        std::fs::remove_dir_all(path).ok();
        git(repo, &["worktree", "prune"])
            .with_context(|| format!("git refused to remove the worktree ({original}), and"))?;
        return Ok(());
    }
    result
        .map(|_| ())
        .with_context(|| worktree_removal_hint(path))
}

/// A plain-language hint for the most common `git worktree remove` failure.
///
/// Something the agent started can still be holding the checkout open. The
/// sweep at stop time catches process groups still running under the CLI, but
/// not one that had already left its subtree — a job backgrounded with `&`
/// included (see `agent::process::ChildHandle::stop`).
fn worktree_removal_hint(path: &Path) -> String {
    format!(
        "could not remove the worktree at {}. If a process the agent started is still \
         running there (a build, a dev server, or something backgrounded with `&`), \
         stop it and try again, or delete anyway to force it",
        path.display()
    )
}

/// In-place branching for the "work in the main checkout instead" toggle.
///
/// `git checkout -b <branch> <start-point>` has no `--` form for the start
/// point (`--` introduces paths there), so the base is resolved to an object id
/// before it is passed. Without that, a base of `--force` would silently throw
/// away the user's uncommitted changes.
pub fn create_branch_in_place(repo: &Path, branch: &str, base_ref: Option<&str>) -> Result<()> {
    validate_ref(branch)?;
    let base_oid = base_ref
        .map(|base| resolve_commit(repo, base))
        .transpose()?;
    let mut args = vec!["checkout", "-b", branch];
    if let Some(oid) = &base_oid {
        args.push(oid);
    }
    git(repo, &args)?;
    Ok(())
}

/// Delete a branch. Without `force` git refuses to drop unmerged work.
pub fn delete_branch(repo: &Path, branch: &str, force: bool) -> Result<()> {
    validate_ref(branch)?;
    git(
        repo,
        &["branch", if force { "-D" } else { "-d" }, "--", branch],
    )?;
    Ok(())
}

/// What deleting this agent would throw away.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SafetyReport {
    /// `git status --porcelain` lines in the worktree.
    pub uncommitted: Vec<String>,
    /// Commits on the branch that are on no remote.
    pub unpushed: Vec<String>,
    /// The branch has no commits beyond its base, or they are already merged.
    pub branch_empty_or_merged: bool,
    /// True when nothing would be lost.
    pub safe: bool,
    /// Why the check could not be completed. A git failure lands here and
    /// forces `safe = false`: the check fails closed, never open.
    pub error: Option<String>,
}

impl SafetyReport {
    /// A report for a check that could not be completed. Never safe.
    pub fn failed(err: impl std::fmt::Display) -> Self {
        Self {
            uncommitted: Vec::new(),
            unpushed: Vec::new(),
            branch_empty_or_merged: false,
            safe: false,
            error: Some(err.to_string()),
        }
    }

    /// A one-line human summary of what stands in the way.
    pub fn blocker(&self) -> Option<String> {
        if self.safe {
            return None;
        }
        if let Some(err) = &self.error {
            return Some(format!("the safety check could not be completed ({err})"));
        }
        let mut parts = Vec::new();
        if !self.uncommitted.is_empty() {
            parts.push(format!("{} uncommitted change(s)", self.uncommitted.len()));
        }
        if !self.unpushed.is_empty() {
            parts.push(format!("{} unpushed commit(s)", self.unpushed.len()));
        }
        if parts.is_empty() {
            return Some("the worktree is not clean".to_string());
        }
        Some(parts.join(" and "))
    }
}

/// Inspect a worktree and its branch before deleting an agent (§6).
///
/// Never commits, never pushes: it only reports. Any git failure is an error,
/// not an empty result — a check that cannot run must not read as "safe".
pub fn safety_report(
    work_path: &Path,
    repo: &Path,
    branch: Option<&str>,
    base_ref: Option<&str>,
) -> Result<SafetyReport> {
    let uncommitted = lines(
        &git(work_path, &["status", "--porcelain"])
            .context("checking the worktree for uncommitted changes")?,
    );

    let unpushed = match branch {
        Some(branch) => {
            let branch_oid = resolve_commit(repo, branch)
                .with_context(|| format!("resolving the agent's branch `{branch}`"))?;
            let range = match base_ref {
                Some(base) => {
                    let base_oid = resolve_commit(repo, base)
                        .with_context(|| format!("resolving the base ref `{base}`"))?;
                    format!("{base_oid}..{branch_oid}")
                }
                None => branch_oid,
            };
            lines(
                &git(
                    repo,
                    &[
                        "log",
                        "--oneline",
                        "--no-decorate",
                        &range,
                        "--not",
                        "--remotes",
                    ],
                )
                .context("looking for unpushed commits")?,
            )
        }
        None => Vec::new(),
    };

    let branch_empty_or_merged = unpushed.is_empty();
    let safe = uncommitted.is_empty() && unpushed.is_empty();
    Ok(SafetyReport {
        uncommitted,
        unpushed,
        branch_empty_or_merged,
        safe,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn slugify_lowercases_and_replaces_non_alphanumerics() {
        assert_eq!(slugify("Fix the parser"), "fix_the_parser");
        assert_eq!(slugify("Add OAuth2 support!"), "add_oauth2_support");
        assert_eq!(slugify("UPPER/lower"), "upper_lower");
        assert_eq!(slugify("a-b.c_d"), "a_b_c_d");
    }

    #[test]
    fn slugify_collapses_runs_and_trims_edges() {
        assert_eq!(slugify("  spaced   out  "), "spaced_out");
        assert_eq!(slugify("---dashes---"), "dashes");
        assert_eq!(slugify("a!!!b"), "a_b");
    }

    #[test]
    fn slugify_caps_length() {
        let long = "word ".repeat(40);
        let slug = slugify(&long);
        assert!(slug.chars().count() <= MAX_SLUG_LEN, "{slug}");
        assert!(slug.starts_with("word_word"));
    }

    #[test]
    fn slugify_never_returns_empty() {
        assert_eq!(slugify(""), "agent");
        assert_eq!(slugify("!!!"), "agent");
        assert_eq!(slugify("   "), "agent");
    }

    #[test]
    fn slugify_handles_non_ascii() {
        assert_eq!(slugify("café ☕ time"), "caf_time");
        assert_eq!(slugify("日本語"), "agent");
    }

    #[test]
    fn branch_name_uses_the_configured_prefix() {
        assert_eq!(branch_name("sw_", "fix_it"), "sw_fix_it");
        assert_eq!(branch_name("", "fix_it"), "fix_it");
    }

    #[test]
    fn unique_name_suffixes_on_collision() {
        let taken = set(&["fix_it", "fix_it_2", "fix_it_3"]);
        assert_eq!(unique_name("other", |c| taken.contains(c)), "other");
        assert_eq!(unique_name("fix_it", |c| taken.contains(c)), "fix_it_4");
    }

    #[test]
    fn allocate_names_keeps_slug_and_branch_in_step() {
        let slugs = set(&["fix_the_parser"]);
        let branches = HashSet::new();
        let (slug, branch) = allocate_names("Fix the parser", "sw_", &slugs, &branches);
        assert_eq!(slug, "fix_the_parser_2");
        assert_eq!(branch, "sw_fix_the_parser_2");
    }

    #[test]
    fn allocate_names_avoids_existing_git_branches_too() {
        let slugs = HashSet::new();
        let branches = set(&["sw_fix_the_parser", "sw_fix_the_parser_2"]);
        let (slug, branch) = allocate_names("Fix the parser", "sw_", &slugs, &branches);
        assert_eq!(slug, "fix_the_parser_3");
        assert_eq!(branch, "sw_fix_the_parser_3");
    }

    #[test]
    fn allocate_names_is_a_no_op_when_nothing_collides() {
        let (slug, branch) = allocate_names("Fresh task", "sw_", &HashSet::new(), &HashSet::new());
        assert_eq!(slug, "fresh_task");
        assert_eq!(branch, "sw_fresh_task");
    }

    #[test]
    fn worktree_path_lives_under_a_dot_directory() {
        let p = worktree_path(Path::new("/root"), "myrepo", "fix_it");
        assert_eq!(p, PathBuf::from("/root/.worktrees/myrepo/fix_it"));
    }

    #[test]
    fn safety_report_summarises_blockers() {
        let clean = SafetyReport {
            safe: true,
            ..Default::default()
        };
        assert_eq!(clean.blocker(), None);

        let dirty = SafetyReport {
            uncommitted: vec![" M src/main.rs".into()],
            unpushed: vec!["abc123 wip".into(), "def456 more".into()],
            branch_empty_or_merged: false,
            safe: false,
            error: None,
        };
        assert_eq!(
            dirty.blocker().as_deref(),
            Some("1 uncommitted change(s) and 2 unpushed commit(s)")
        );
    }

    // -- tests against a real, local, throwaway repository -------------------

    struct TestRepo {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    fn init_repo() -> Option<TestRepo> {
        let dir = tempfile::tempdir().ok()?;
        let path = dir.path().join("repo");
        std::fs::create_dir_all(&path).ok()?;
        let cfg: &[&[&str]] = &[
            &["init", "-q", "-b", "main"],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
            &["config", "commit.gpgsign", "false"],
        ];
        for args in cfg {
            git(&path, args).ok()?;
        }
        std::fs::write(path.join("README.md"), "hello\n").ok()?;
        git(&path, &["add", "."]).ok()?;
        git(&path, &["commit", "-q", "-m", "initial"]).ok()?;
        Some(TestRepo { _dir: dir, path })
    }

    #[test]
    fn reads_metadata_from_a_real_repo() {
        let Some(repo) = init_repo() else {
            return; // no usable git binary; the pure logic above still covers naming
        };
        assert!(is_git_repo(&repo.path));
        assert!(!is_git_repo(repo.path.parent().unwrap_or(&repo.path)));
        assert_eq!(current_branch(&repo.path).as_deref(), Some("main"));
        assert!(!is_dirty(&repo.path));
        std::fs::write(repo.path.join("new.txt"), "x").expect("write");
        assert!(is_dirty(&repo.path));
        assert_eq!(list_branches(&repo.path), vec!["main".to_string()]);
    }

    #[test]
    fn worktree_add_and_remove_round_trip() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wt = worktree_path(&root, "repo", "fix_it");
        add_worktree(&repo.path, &wt, "sw_fix_it", Some("main")).expect("add worktree");
        assert!(wt.join("README.md").exists());
        assert_eq!(current_branch(&wt).as_deref(), Some("sw_fix_it"));

        // A second add on the same path is refused rather than clobbering.
        assert!(add_worktree(&repo.path, &wt, "sw_other", Some("main")).is_err());

        let report =
            safety_report(&wt, &repo.path, Some("sw_fix_it"), Some("main")).expect("report");
        assert!(report.safe, "fresh worktree should be safe: {report:?}");
        assert!(report.branch_empty_or_merged);

        remove_worktree(&repo.path, &wt, false).expect("remove worktree");
        assert!(!wt.exists());
    }

    #[test]
    fn safety_report_flags_uncommitted_and_unpushed_work() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wt = worktree_path(&root, "repo", "risky");
        add_worktree(&repo.path, &wt, "sw_risky", Some("main")).expect("add worktree");

        std::fs::write(wt.join("scratch.txt"), "wip").expect("write");
        let report =
            safety_report(&wt, &repo.path, Some("sw_risky"), Some("main")).expect("report");
        assert!(!report.safe);
        assert_eq!(report.uncommitted.len(), 1);
        assert!(report.blocker().is_some_and(|b| b.contains("uncommitted")));

        git(&wt, &["add", "."]).expect("add");
        git(&wt, &["commit", "-q", "-m", "wip"]).expect("commit");
        let report =
            safety_report(&wt, &repo.path, Some("sw_risky"), Some("main")).expect("report");
        assert!(report.uncommitted.is_empty());
        assert_eq!(report.unpushed.len(), 1, "{report:?}");
        assert!(!report.safe);
        assert!(!report.branch_empty_or_merged);

        // Forced removal drops the worktree despite the dirty history.
        remove_worktree(&repo.path, &wt, true).expect("force remove");
        assert!(!wt.exists());
        delete_branch(&repo.path, "sw_risky", true).expect("delete branch");
        assert!(!list_branches(&repo.path).contains(&"sw_risky".to_string()));
    }

    #[test]
    fn in_place_branching_switches_the_main_checkout() {
        let Some(repo) = init_repo() else { return };
        create_branch_in_place(&repo.path, "sw_inplace", Some("main")).expect("checkout -b");
        assert_eq!(current_branch(&repo.path).as_deref(), Some("sw_inplace"));
    }

    #[test]
    fn option_shaped_refs_are_rejected() {
        for bad in [
            "--force",
            "-b",
            "--output=/tmp/pwned_log.txt",
            "",
            "a..b",
            "with space",
            "tilde~1",
            "caret^",
            "colon:name",
            "star*",
            "back\\slash",
            "trailing/",
            "/leading",
            "at@{0}",
            "ends.",
            "ref.lock",
        ] {
            assert!(validate_ref(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in [
            "main",
            "master",
            "origin/main",
            "sw_fix_it",
            "v1.2.3",
            "HEAD",
            "release/2024",
        ] {
            assert!(validate_ref(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn option_shaped_branch_prefixes_are_rejected() {
        for bad in ["--force", "-x", "/abs", "a..b", "with space", "tilde~"] {
            assert!(
                validate_branch_prefix(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        for good in ["", "sw_", "feature/", "claude-"] {
            assert!(
                validate_branch_prefix(good).is_ok(),
                "{good:?} must be accepted"
            );
        }
    }

    #[test]
    fn option_shaped_paths_are_rejected() {
        assert!(checked_path(Path::new("--upload-pack=x")).is_err());
        assert!(checked_path(Path::new("/tmp/fine")).is_ok());
    }

    #[test]
    fn an_option_shaped_base_ref_never_reaches_git() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let sentinel = root.join("pwned_log.txt");

        // §6's checkout path: `--force` here would silently discard uncommitted work.
        std::fs::write(repo.path.join("README.md"), "locally modified\n").expect("write");
        let err = create_branch_in_place(&repo.path, "sw_injected", Some("--force"))
            .expect_err("must be refused");
        assert!(
            format!("{err:#}").contains("would read as an option"),
            "{err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path.join("README.md")).expect("read"),
            "locally modified\n",
            "the working tree must be untouched"
        );

        // The delete-preview path: `--output=<file>` here would write a file.
        let out = format!("--output={}", sentinel.display());
        let err = safety_report(&repo.path, &repo.path, Some("main"), Some(&out))
            .expect_err("must be refused");
        assert!(
            format!("{err:#}").contains("would read as an option"),
            "{err:#}"
        );
        assert!(!sentinel.exists(), "git must never have run");

        let err = add_worktree(&repo.path, &root.join("wt"), "sw_x", Some("--force"))
            .expect_err("refused");
        assert!(
            format!("{err:#}").contains("would read as an option"),
            "{err:#}"
        );
    }

    #[test]
    fn a_ref_that_does_not_exist_is_an_error_not_a_silent_default() {
        let Some(repo) = init_repo() else { return };
        assert!(resolve_commit(&repo.path, "no_such_branch").is_err());
        let oid = resolve_commit(&repo.path, "main").expect("resolve");
        assert_eq!(oid.len(), 40, "{oid}");
        assert!(oid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn safety_report_fails_closed_when_git_fails() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wt = worktree_path(&root, "repo", "closed");
        add_worktree(&repo.path, &wt, "sw_closed", Some("main")).expect("add worktree");
        std::fs::write(wt.join("f.txt"), "work").expect("write");
        git(&wt, &["add", "."]).expect("add");
        git(&wt, &["commit", "-q", "-m", "unpushed work"]).expect("commit");

        // The base branch is renamed out from under the agent, so the range
        // cannot be resolved. That must not read as "nothing would be lost".
        git(&repo.path, &["branch", "-m", "main", "trunk"]).expect("rename");
        let err = safety_report(&wt, &repo.path, Some("sw_closed"), Some("main"))
            .expect_err("a failed check must be an error");
        let report = SafetyReport::failed(format!("{err:#}"));
        assert!(!report.safe);
        assert!(!report.branch_empty_or_merged);
        let blocker = report.blocker().unwrap_or_default();
        assert!(blocker.contains("could not be completed"), "{blocker}");
        assert!(
            report.error.unwrap_or_default().contains("main"),
            "the git failure must be surfaced to the operator"
        );
    }

    #[test]
    fn a_refused_worktree_removal_surfaces_gits_own_words() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wt = worktree_path(&root, "repo", "busy");
        add_worktree(&repo.path, &wt, "sw_busy", Some("main")).expect("add worktree");
        std::fs::write(wt.join("build-output.txt"), "still here").expect("write");

        let err = remove_worktree(&repo.path, &wt, false).expect_err("git must refuse");
        let text = format!("{err:#}");
        assert!(
            text.contains("could not remove the worktree"),
            "the operator needs to know which worktree: {text}"
        );
        assert!(
            text.to_lowercase().contains("untracked") || text.to_lowercase().contains("modified"),
            "git's own reason must be passed through, not flattened: {text}"
        );
        assert!(
            text.contains("delete anyway"),
            "and how to get past it: {text}"
        );
        assert!(wt.exists(), "a refused removal must change nothing");

        // Forcing does remove it, and says nothing misleading on success.
        remove_worktree(&repo.path, &wt, true).expect("force remove");
        assert!(!wt.exists());
    }

    #[test]
    fn forcing_a_worktree_whose_directory_is_already_gone_still_prunes() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wt = worktree_path(&root, "repo", "vanished");
        add_worktree(&repo.path, &wt, "sw_vanished", Some("main")).expect("add worktree");

        // Something removed the directory behind git's back.
        std::fs::remove_dir_all(&wt).expect("remove directory");
        assert!(
            git(&repo.path, &["worktree", "list"])
                .expect("list")
                .contains("vanished"),
            "git should still be holding the registration"
        );

        remove_worktree(&repo.path, &wt, true).expect("force remove");
        assert!(
            !git(&repo.path, &["worktree", "list"])
                .expect("list")
                .contains("vanished"),
            "the stale registration must be pruned, not left behind"
        );
    }

    /// Plant every class of config key that turns `git` into a command runner —
    /// including a clean/smudge filter driver, which `git status` runs when it
    /// has to re-hash a working-tree file, and which no fixed `-c` list covers
    /// because the driver's name is the attacker's to choose.
    #[test]
    fn a_poisoned_repository_config_never_executes() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let sentinel = root.join("PWNED");
        let payload = root.join("payload.sh");
        std::fs::write(
            &payload,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        )
        .expect("write payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        // Files the filters apply to, committed before the drivers are planted.
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(repo.path.join(name), "hello\n").expect("write");
        }
        std::fs::write(
            repo.path.join(".gitattributes"),
            "a.txt filter=evil\nb.txt filter=included\nc.txt filter=worktree\n",
        )
        .expect("write attributes");
        git(&repo.path, &["add", "."]).expect("add");
        git(&repo.path, &["commit", "-q", "-m", "with attributes"]).expect("commit");

        // A filter hidden behind an include: `.git/config` shows only the
        // include line, which is ordinary data. Without expanding includes the
        // guard sees nothing while git honours the driver.
        let included = repo.path.join(".git").join("hidden.cfg");
        std::fs::write(
            &included,
            format!(
                "[filter \"included\"]\n\tclean = {p}\n\tsmudge = {p}\n",
                p = payload.display()
            ),
        )
        .expect("write included config");

        // And one in the per-worktree config, which `--local` never reads.
        std::fs::write(
            repo.path.join(".git").join("config.worktree"),
            format!(
                "[filter \"worktree\"]\n\tclean = {p}\n\tsmudge = {p}\n",
                p = payload.display()
            ),
        )
        .expect("write worktree config");

        // Straight into .git/config, exactly as an unzipped directory would
        // arrive. Not via `git config`, so nothing sanitises it on the way in.
        let config = repo.path.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).expect("read config");
        text.push_str(&format!(
            "\n[core]\n\tfsmonitor = {p}\n\tsshCommand = {p}\n\tpager = {p}\n\thooksPath = {dir}\n\tgitProxy = {p}\n\
             \n[filter \"evil\"]\n\tclean = {p}\n\tsmudge = {p}\n\
             \n[diff]\n\texternal = {p}\n\
             \n[uploadpack]\n\tpackObjectsHook = {p}\n\
             \n[remote \"origin\"]\n\turl = {dir}\n\
             \n[alias]\n\tpwn = !{p}\n\
             \n[include]\n\tpath = {inc}\n\
             \n[extensions]\n\tworktreeConfig = true\n",
            p = payload.display(),
            dir = root.display(),
            inc = included.display(),
        ));
        std::fs::write(&config, text).expect("write config");

        // Three drivers now apply to the same file: one written directly, one
        // reached through the include, one in the worktree config.
        std::fs::write(
            repo.path.join(".gitattributes"),
            "a.txt filter=evil\nb.txt filter=included\nc.txt filter=worktree\n",
        )
        .expect("write attributes");

        // Same size, stale timestamp: git cannot decide from stat data alone
        // and has to re-hash the file — which is what runs the clean filter.
        let stale = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(repo.path.join(name), "HELLO\n").expect("modify");
            let _ = filetime(&repo.path.join(name), stale);
        }

        // Everything an endpoint or the scanner can reach.
        let _ = is_git_repo(&repo.path);
        let _ = current_branch(&repo.path);
        let _ = is_dirty(&repo.path);
        let _ = list_branches(&repo.path);
        let _ = repo_metadata(&repo.path);
        let _ = resolve_commit(&repo.path, "main");
        let _ = safety_report(&repo.path, &repo.path, Some("main"), None);
        let _ = fetch(&repo.path);
        let wt = root.join("wt");
        let _ = add_worktree(&repo.path, &wt, "sw_poisoned", Some("main"));
        let _ = remove_worktree(&repo.path, &wt, true);

        assert!(
            !sentinel.exists(),
            "a config key from the inspected repository executed a command"
        );

        // And the disarming is real, not incidental: the guard names each key.
        let guard = RepoGuard::read(&repo.path);
        for key in [
            "core.fsmonitor=",
            "core.sshcommand=",
            "core.gitproxy=",
            "core.hookspath=/dev/null",
            "core.pager=cat",
            "filter.evil.clean=",
            "filter.evil.smudge=",
            "filter.included.clean=",
            "filter.included.smudge=",
            "filter.worktree.clean=",
            "filter.worktree.smudge=",
            "diff.external=",
            "uploadpack.packobjectshook=",
            "alias.pwn=",
        ] {
            assert!(
                guard.overrides.iter().any(|o| o == key),
                "{key} should have been disarmed: {:?}",
                guard.overrides
            );
        }
        assert!(guard.refusals.is_empty(), "{:?}", guard.refusals);
    }

    /// Set a file's modification time, so git cannot trust its cached stat data.
    fn filetime(path: &Path, when: std::time::SystemTime) -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_modified(when)
    }

    #[test]
    fn a_repository_declaring_a_command_we_cannot_disarm_is_refused() {
        let Some(repo) = init_repo() else { return };
        let config = repo.path.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).expect("read config");
        // A namespace we know nothing about, with a command-shaped key. There
        // is no `-c` override that makes this provably inert, so the repository
        // is not touched at all.
        text.push_str("\n[sometool \"x\"]\n\tcommand = /tmp/payload.sh\n");
        std::fs::write(&config, text).expect("write config");

        let guard = RepoGuard::read(&repo.path);
        assert!(
            guard.refusals.iter().any(|r| r.contains("sometool")),
            "{:?}",
            guard.refusals
        );

        let err = git(&repo.path, &["status", "--porcelain"]).expect_err("must refuse");
        assert!(format!("{err:#}").contains("runs commands"), "{err:#}");

        // The scanner reports it rather than running it.
        let meta = repo_metadata(&repo.path);
        assert!(meta.refused.is_some());
        assert_eq!(meta.branch, None);
        assert!(!meta.dirty);
    }

    #[test]
    fn a_remote_upload_pack_command_never_runs_on_fetch() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let sentinel = root.join("UPLOADPACK_PWNED");
        let payload = root.join("up.sh");
        std::fs::write(
            &payload,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        )
        .expect("write payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let config = repo.path.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).expect("read config");
        text.push_str(&format!(
            "\n[remote \"origin\"]\n\turl = {}\n\tuploadpack = {}\n",
            repo.path.display(),
            payload.display()
        ));
        std::fs::write(&config, text).expect("write config");

        // Not a refusal: a custom upload-pack is a legitimate thing for a
        // repository to declare, and refusing would make it unusable. `-c`
        // provably does not override this key, so `fetch` pins the argument
        // that does.
        let guard = RepoGuard::read(&repo.path);
        assert!(guard.refusals.is_empty(), "{:?}", guard.refusals);
        let _ = fetch(&repo.path);
        assert!(
            !sentinel.exists(),
            "a remote's upload-pack command must never run"
        );
    }

    /// The fixed `SAFE_CONFIG` list is the backstop for anything the per-repo
    /// guard does not see, so it has to work on its own.
    ///
    /// `git_raw` is the runner that applies only that list — no per-repository
    /// disarming — which is what makes this a test of the static entry rather
    /// than of the guard. The control is the mutation: remove
    /// `-c core.fsmonitor=` from `SAFE_CONFIG` and this fails.
    #[test]
    fn the_fixed_list_alone_suppresses_a_declared_fsmonitor() {
        let Some(repo) = init_repo() else { return };
        let root = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let sentinel = root.join("FSMONITOR_PWNED");
        let payload = root.join("fsmon.sh");
        std::fs::write(
            &payload,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        )
        .expect("write payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let config = repo.path.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).expect("read config");
        text.push_str(&format!("\n[core]\n\tfsmonitor = {}\n", payload.display()));
        std::fs::write(&config, text).expect("write config");

        std::fs::write(repo.path.join("README.md"), "changed\n").expect("modify");
        let _ = git_raw(&repo.path, &[], &["status", "--porcelain"]);
        assert!(
            !sentinel.exists(),
            "the fixed list must suppress core.fsmonitor by itself"
        );
    }

    #[test]
    fn a_repository_whose_config_cannot_be_read_is_refused() {
        let Some(repo) = init_repo() else { return };
        // git will not parse this, so we cannot vet what it declares. Running
        // it under the fixed list alone is exactly the fail-open case the
        // guard exists to remove.
        std::fs::write(
            repo.path.join(".git").join("config"),
            "[core\nthis is not valid config\n",
        )
        .expect("write config");

        let guard = RepoGuard::read(&repo.path);
        assert!(!guard.refusals.is_empty(), "{guard:?}");
        let err = git(&repo.path, &["status", "--porcelain"]).expect_err("must refuse");
        assert!(format!("{err:#}").contains("could not be read"), "{err:#}");

        // A plain directory is not a repository and needs no guard at all.
        let plain = repo
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        assert_eq!(RepoGuard::read(&plain), RepoGuard::default());
    }

    #[test]
    fn an_ordinary_repository_is_neither_disarmed_nor_refused() {
        let Some(repo) = init_repo() else { return };
        let guard = RepoGuard::read(&repo.path);
        assert!(guard.refusals.is_empty(), "{:?}", guard.refusals);
        assert!(guard.overrides.is_empty(), "{:?}", guard.overrides);
        assert_eq!(current_branch(&repo.path).as_deref(), Some("main"));
    }

    #[test]
    fn the_config_classifier_knows_commands_from_data() {
        let listing = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| format!("{k}\n{v}\0"))
                .collect::<String>()
        };

        // Ordinary configuration is left alone.
        let guard = RepoGuard::from_listing(&listing(&[
            ("core.repositoryformatversion", "0"),
            ("core.filemode", "true"),
            ("remote.origin.url", "https://example.com/x.git"),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            ("branch.main.remote", "origin"),
            ("user.email", "someone@example.com"),
            ("http.proxy", "http://proxy.example:3128"),
            ("submodule.lib.url", "https://example.com/lib.git"),
            // An include is data. What matters is the keys it pulls in, and
            // those are read by expanding includes rather than by trying to
            // classify the include itself.
            ("include.path", "/etc/some.cfg"),
            ("extensions.worktreeconfig", "true"),
        ]));
        assert_eq!(guard, RepoGuard::default());

        // Command-valued keys are disarmed by their own names, whatever the
        // attacker called the driver — and with the value that actually makes
        // each inert, which for hooks is not the empty string.
        let guard = RepoGuard::from_listing(&listing(&[
            ("filter.WeIrD-name.clean", "/tmp/x.sh"),
            ("core.gitProxy", "/tmp/x.sh"),
            ("core.hooksPath", "/tmp/hooks"),
            ("submodule.lib.update", "!/tmp/x.sh"),
        ]));
        assert!(guard.refusals.is_empty(), "{:?}", guard.refusals);
        assert_eq!(
            guard.overrides,
            vec![
                "filter.weird-name.clean=",
                "core.gitproxy=",
                "core.hookspath=/dev/null",
                "submodule.lib.update=",
            ]
        );
        assert_eq!(guard.args().len(), 8, "two argv entries per override");

        // `-c` does not override a remote's upload-pack; `fetch` pins the
        // argument that does, so it is neither disarmed nor refused here.
        let guard = RepoGuard::from_listing(&listing(&[
            ("remote.upstream.uploadpack", "/tmp/x.sh"),
            ("remote.upstream.receivepack", "/tmp/x.sh"),
        ]));
        assert_eq!(guard, RepoGuard::default());

        // Anything command-shaped in a namespace we do not understand refuses.
        for (key, value) in [
            ("weirdtool.x.command", "/tmp/x.sh"),
            ("nonsense.pager", "/tmp/x.sh"),
            ("whatever.key", "!/tmp/x.sh"),
            ("some.url", "ext::sh -c whatever"),
        ] {
            let guard = RepoGuard::from_listing(&listing(&[(key, value)]));
            assert!(
                !guard.refusals.is_empty(),
                "{key} = {value} should refuse, got {guard:?}"
            );
        }
    }
}
