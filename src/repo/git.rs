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

// ---------------------------------------------------------------------------
// git plumbing
// ---------------------------------------------------------------------------

/// Run `git` in `cwd`, returning trimmed stdout. Fails with git's stderr.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
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
pub fn fetch(repo: &Path) -> Result<String> {
    git(repo, &["fetch", "--all", "--prune"])
}

/// `git worktree add <path> -b <branch> [<base_ref>]`.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base_ref: Option<&str>) -> Result<()> {
    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8"))?;
    let mut args = vec!["worktree", "add", path_str, "-b", branch];
    if let Some(base) = base_ref {
        args.push(base);
    }
    git(repo, &args)?;
    Ok(())
}

/// `git worktree remove`, then prune the administrative files.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8"))?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);
    let result = git(repo, &args);
    if result.is_err() && force {
        // The checkout may already be gone; drop the registration either way.
        std::fs::remove_dir_all(path).ok();
        git(repo, &["worktree", "prune"])?;
        return Ok(());
    }
    result.map(|_| ())
}

/// In-place branching for the "work in the main checkout instead" toggle.
pub fn create_branch_in_place(repo: &Path, branch: &str, base_ref: Option<&str>) -> Result<()> {
    let mut args = vec!["checkout", "-b", branch];
    if let Some(base) = base_ref {
        args.push(base);
    }
    git(repo, &args)?;
    Ok(())
}

/// Delete a branch. Without `force` git refuses to drop unmerged work.
pub fn delete_branch(repo: &Path, branch: &str, force: bool) -> Result<()> {
    git(repo, &["branch", if force { "-D" } else { "-d" }, branch])?;
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
}

impl SafetyReport {
    /// A one-line human summary of what stands in the way.
    pub fn blocker(&self) -> Option<String> {
        if self.safe {
            return None;
        }
        let mut parts = Vec::new();
        if !self.uncommitted.is_empty() {
            parts.push(format!("{} uncommitted change(s)", self.uncommitted.len()));
        }
        if !self.unpushed.is_empty() {
            parts.push(format!("{} unpushed commit(s)", self.unpushed.len()));
        }
        Some(parts.join(" and "))
    }
}

/// Inspect a worktree and its branch before deleting an agent (§6).
///
/// Never commits, never pushes: it only reports.
pub fn safety_report(
    work_path: &Path,
    repo: &Path,
    branch: Option<&str>,
    base_ref: Option<&str>,
) -> SafetyReport {
    let uncommitted = git_opt(work_path, &["status", "--porcelain"])
        .map(|s| lines(&s))
        .unwrap_or_default();

    let unpushed = match branch {
        Some(branch) => {
            let range = base_ref.map(|base| format!("{base}..{branch}"));
            let mut args: Vec<&str> = vec!["log", "--oneline", "--no-decorate"];
            match &range {
                Some(r) => args.push(r),
                None => args.push(branch),
            }
            args.push("--not");
            args.push("--remotes");
            git_opt(repo, &args).map(|s| lines(&s)).unwrap_or_default()
        }
        None => Vec::new(),
    };

    let branch_empty_or_merged = unpushed.is_empty();
    let safe = uncommitted.is_empty() && unpushed.is_empty();
    SafetyReport {
        uncommitted,
        unpushed,
        branch_empty_or_merged,
        safe,
    }
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

        let report = safety_report(&wt, &repo.path, Some("sw_fix_it"), Some("main"));
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
        let report = safety_report(&wt, &repo.path, Some("sw_risky"), Some("main"));
        assert!(!report.safe);
        assert_eq!(report.uncommitted.len(), 1);
        assert!(report.blocker().is_some_and(|b| b.contains("uncommitted")));

        git(&wt, &["add", "."]).expect("add");
        git(&wt, &["commit", "-q", "-m", "wip"]).expect("commit");
        let report = safety_report(&wt, &repo.path, Some("sw_risky"), Some("main"));
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
}
