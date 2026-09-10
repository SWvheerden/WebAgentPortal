//! Repo-root scanning: immediate children only, git metadata, recency ordering.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::git;

/// How many repos the "Recent" group holds.
pub const RECENT_LIMIT: usize = 5;

/// One directory found under a configured repo root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    pub path: String,
    pub root: String,
    pub is_git: bool,
    pub branch: Option<String>,
    pub dirty: bool,
    /// Set when the repository's own git config declares commands, so it was
    /// not inspected. Spawning into it is refused for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// This entry *is* a configured root, offered as a workspace in its own
    /// right rather than as one of the repositories under it (§6).
    ///
    /// A root spawn is deliberately rootless: `is_git` is always false on these
    /// entries, whatever the directory itself happens to contain, because a
    /// root is a container of repositories and an agent given one is not tied
    /// to any of them.
    #[serde(default)]
    pub is_root: bool,
    /// This tool's own last-used timestamp, not the filesystem's.
    pub last_used_at: Option<i64>,
}

/// The picker payload: a Recent group and everything, alphabetically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoListing {
    /// The configured roots themselves, in configured order. Kept out of
    /// `recent` and `all` because a root is not one of the repositories the
    /// picker is ordering — it is the folder they all sit in.
    #[serde(default)]
    pub roots: Vec<RepoEntry>,
    pub recent: Vec<RepoEntry>,
    pub all: Vec<RepoEntry>,
    /// Roots that could not be read, surfaced rather than silently dropped.
    pub errors: Vec<String>,
}

/// Scan every configured root. Blocking: call from `spawn_blocking`.
///
/// Immediate children only, dot-directories skipped, symlinks followed but not
/// recursed. Plain directories are listed alongside git ones — they are real
/// workspaces (§6).
pub fn scan_roots(roots: &[PathBuf], usage: &HashMap<String, i64>) -> RepoListing {
    let mut entries: Vec<RepoEntry> = Vec::new();
    let mut errors = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    // The roots first, so a root nested inside another root is offered as the
    // root it is configured as rather than as a repository under its parent.
    // A root that is not a readable directory is left out here and reported as
    // an error below: offering a workspace that cannot be entered would only
    // fail at spawn time.
    let mut root_entries: Vec<RepoEntry> = Vec::with_capacity(roots.len());
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let entry = root_entry(root, usage);
        if !seen.contains(&entry.path) {
            seen.push(entry.path.clone());
            root_entries.push(entry);
        }
    }

    for root in roots {
        match scan_root(root, usage) {
            Ok(found) => {
                for entry in found {
                    if !seen.contains(&entry.path) {
                        seen.push(entry.path.clone());
                        entries.push(entry);
                    }
                }
            }
            Err(err) => errors.push(format!("{}: {err}", root.display())),
        }
    }

    order(entries, root_entries, errors)
}

/// The picker entry for a configured root itself (§6).
///
/// No git *command* is run against it: a root spawn has no branch or worktree
/// semantics whatever the directory contains, and reporting a branch here would
/// offer the operator a choice the spawn does not honour.
///
/// The config-only vet of §7 is the exception, and has to be. A root that is
/// itself a working tree can declare a command-valued key we cannot disarm, and
/// the agent's own CLI runs git in its cwd on startup — so "we run no git" is
/// not the same as "no git runs". `git config --list` reads config files and no
/// working tree, so asking costs nothing the guard exists to prevent.
fn root_entry(root: &Path, usage: &HashMap<String, i64>) -> RepoEntry {
    let path = root.to_string_lossy().to_string();
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    // Free for a plain root: the guard short-circuits when there is no `.git`.
    let refused = git::RepoGuard::read(root)
        .check(root)
        .err()
        .map(|err| format!("{err}"));
    RepoEntry {
        name,
        branch: None,
        dirty: false,
        refused,
        is_git: false,
        is_root: true,
        last_used_at: usage.get(&path).copied(),
        root: path.clone(),
        path,
    }
}

fn scan_root(root: &Path, usage: &HashMap<String, i64>) -> std::io::Result<Vec<RepoEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        // Follows symlinks by design; we never recurse into what we find.
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_git = git::is_git_repo(&path);
        // One guarded pass per repository: a repository whose config declares
        // commands is reported, not run.
        let meta = if is_git {
            git::repo_metadata(&path)
        } else {
            git::RepoMeta::default()
        };
        let path_str = path.to_string_lossy().to_string();
        out.push(RepoEntry {
            name,
            branch: meta.branch,
            dirty: meta.dirty,
            refused: meta.refused,
            is_git,
            is_root: false,
            last_used_at: usage.get(&path_str).copied(),
            path: path_str,
            root: root.to_string_lossy().to_string(),
        });
    }
    Ok(out)
}

/// Split into a recency-ordered Recent group and an alphabetical All list.
///
/// Pure, so the ordering rules are testable without a filesystem.
pub fn order(
    mut entries: Vec<RepoEntry>,
    roots: Vec<RepoEntry>,
    errors: Vec<String>,
) -> RepoListing {
    entries.sort_by_key(|e| e.name.to_lowercase());

    let mut recent: Vec<RepoEntry> = entries
        .iter()
        .filter(|e| e.last_used_at.is_some())
        .cloned()
        .collect();
    recent.sort_by(|a, b| {
        b.last_used_at
            .cmp(&a.last_used_at)
            // Ties break alphabetically so the order never wobbles.
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    recent.truncate(RECENT_LIMIT);

    RepoListing {
        roots,
        recent,
        all: entries,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, last_used_at: Option<i64>) -> RepoEntry {
        RepoEntry {
            name: name.to_string(),
            path: format!("/root/{name}"),
            root: "/root".to_string(),
            is_git: true,
            branch: Some("main".to_string()),
            dirty: false,
            refused: None,
            is_root: false,
            last_used_at,
        }
    }

    #[test]
    fn all_is_alphabetical_and_case_insensitive() {
        let listing = order(
            vec![
                entry("zeta", None),
                entry("Alpha", None),
                entry("beta", None),
            ],
            vec![],
            vec![],
        );
        let names: Vec<_> = listing.all.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn recent_is_newest_first_and_capped() {
        let entries = (1..=8)
            .map(|i| entry(&format!("repo{i}"), Some(i as i64 * 100)))
            .collect();
        let listing = order(entries, vec![], vec![]);
        assert_eq!(listing.recent.len(), RECENT_LIMIT);
        let names: Vec<_> = listing.recent.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["repo8", "repo7", "repo6", "repo5", "repo4"]);
        assert_eq!(listing.all.len(), 8, "All still lists everything");
    }

    #[test]
    fn never_used_repos_are_absent_from_recent() {
        let listing = order(
            vec![entry("used", Some(5)), entry("never", None)],
            vec![],
            vec![],
        );
        assert_eq!(listing.recent.len(), 1);
        assert_eq!(listing.recent[0].name, "used");
    }

    #[test]
    fn recency_ties_break_alphabetically() {
        let listing = order(
            vec![entry("b", Some(10)), entry("a", Some(10))],
            vec![],
            vec![],
        );
        let names: Vec<_> = listing.recent.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn scans_immediate_children_only_and_skips_dot_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for name in ["alpha", "beta", ".worktrees", ".hidden"] {
            std::fs::create_dir_all(root.join(name)).expect("mkdir");
        }
        // A nested directory must not be listed on its own.
        std::fs::create_dir_all(root.join("alpha").join("nested")).expect("mkdir");
        std::fs::write(root.join("a-file.txt"), "x").expect("write");

        let listing = scan_roots(&[root.to_path_buf()], &HashMap::new());
        let names: Vec<_> = listing.all.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(listing.errors.is_empty());
    }

    #[test]
    fn plain_directories_are_listed_and_badged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("plainfolder")).expect("mkdir");
        let listing = scan_roots(&[root.to_path_buf()], &HashMap::new());
        assert_eq!(listing.all.len(), 1);
        assert!(!listing.all[0].is_git);
        assert_eq!(listing.all[0].branch, None);
        assert!(!listing.all[0].dirty);
    }

    #[test]
    fn a_missing_root_is_reported_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("real")).expect("mkdir");
        let listing = scan_roots(
            &[dir.path().to_path_buf(), dir.path().join("nope")],
            &HashMap::new(),
        );
        assert_eq!(listing.all.len(), 1);
        assert_eq!(listing.errors.len(), 1);
        assert!(listing.errors[0].contains("nope"));
    }

    #[test]
    fn overlapping_roots_do_not_duplicate_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("only")).expect("mkdir");
        let root = dir.path().to_path_buf();
        let listing = scan_roots(&[root.clone(), root], &HashMap::new());
        assert_eq!(listing.all.len(), 1);
    }

    #[test]
    fn the_root_itself_is_offered_as_a_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("alpha")).expect("mkdir");

        let listing = scan_roots(&[root.to_path_buf()], &HashMap::new());
        assert_eq!(
            listing.roots.len(),
            1,
            "the root is spawnable in its own right"
        );
        let entry = &listing.roots[0];
        assert!(entry.is_root);
        assert!(!entry.is_git, "a root carries no git identity");
        assert_eq!(entry.branch, None);
        assert!(!entry.dirty);
        assert_eq!(entry.path, root.to_string_lossy());
        assert_eq!(
            entry.name,
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        );
        // It is not one of the repositories, so it never crowds the lists that
        // order them.
        let names: Vec<_> = listing.all.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
        assert!(listing.recent.is_empty());
    }

    /// A root that is itself a git repository is still offered as a root: the
    /// spawn it feeds runs no git commands, so advertising a branch here would
    /// be a choice nothing honours.
    #[test]
    fn a_git_root_is_still_reported_as_rootless() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        let listing = scan_roots(&[dir.path().to_path_buf()], &HashMap::new());
        assert_eq!(listing.roots.len(), 1);
        assert!(!listing.roots[0].is_git);
        assert!(listing.roots[0].is_root);
    }

    /// A root that declares a command we cannot disarm is badged rather than
    /// entered, exactly as a repository under one is. The `claude` child runs
    /// git in its cwd, so the guard has to reach a root too (§6, §7).
    #[test]
    fn a_root_that_refuses_inspection_is_badged_not_inspected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if git::git(root, &["init", "-q", "-b", "main", "."]).is_err() {
            return;
        }
        let config = root.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).expect("read config");
        text.push_str("\n[sometool \"x\"]\n\tcommand = /tmp/payload.sh\n");
        std::fs::write(&config, text).expect("write config");

        let listing = scan_roots(&[root.to_path_buf()], &HashMap::new());
        assert_eq!(listing.roots.len(), 1);
        let entry = &listing.roots[0];
        assert!(entry.is_root);
        let refused = entry.refused.as_deref().expect("refused");
        assert!(refused.contains("sometool"), "{refused}");
        // And it still says nothing about branches: the vet reads config files,
        // it does not start inspecting the working tree.
        assert!(!entry.is_git);
        assert_eq!(entry.branch, None);
        assert!(!entry.dirty);
    }

    /// The common case pays nothing and is never badged.
    #[test]
    fn an_ordinary_root_is_not_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("alpha")).expect("mkdir");
        let listing = scan_roots(&[dir.path().to_path_buf()], &HashMap::new());
        assert_eq!(listing.roots[0].refused, None);
    }

    #[test]
    fn overlapping_roots_do_not_duplicate_root_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let listing = scan_roots(&[root.clone(), root], &HashMap::new());
        assert_eq!(listing.roots.len(), 1);
    }

    /// A root configured inside another root belongs in the roots group, not in
    /// the repository list of its parent — otherwise selecting it from `all`
    /// would spawn with repository semantics it does not have.
    #[test]
    fn a_nested_root_is_listed_as_a_root_not_as_a_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outer = dir.path().to_path_buf();
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::create_dir_all(outer.join("plain")).expect("mkdir");

        let listing = scan_roots(&[outer, inner.clone()], &HashMap::new());
        let root_paths: Vec<_> = listing.roots.iter().map(|e| e.path.as_str()).collect();
        assert!(root_paths.contains(&inner.to_string_lossy().as_ref()));
        let all_names: Vec<_> = listing.all.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(all_names, vec!["plain"], "`inner` is a root, not a repo");
    }

    /// The listing still describes the roots it could not read: an unusable
    /// root is reported as an error *and* left out of the spawnable group.
    #[test]
    fn a_missing_root_is_not_offered_as_a_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        let listing = scan_roots(&[dir.path().to_path_buf(), missing], &HashMap::new());
        assert_eq!(listing.roots.len(), 1);
        assert_eq!(listing.roots[0].path, dir.path().to_string_lossy());
        assert_eq!(listing.errors.len(), 1);
    }

    #[test]
    fn a_root_carries_its_own_last_used_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut usage = HashMap::new();
        usage.insert(dir.path().to_string_lossy().to_string(), 99_i64);
        let listing = scan_roots(&[dir.path().to_path_buf()], &usage);
        assert_eq!(listing.roots[0].last_used_at, Some(99));
        assert!(
            listing.recent.is_empty(),
            "a root is always visible in its own group, so it never takes a Recent slot"
        );
    }

    #[test]
    fn usage_timestamps_are_attached_by_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("used")).expect("mkdir");
        let mut usage = HashMap::new();
        usage.insert(
            dir.path().join("used").to_string_lossy().to_string(),
            4242_i64,
        );
        let listing = scan_roots(&[dir.path().to_path_buf()], &usage);
        assert_eq!(listing.all[0].last_used_at, Some(4242));
        assert_eq!(listing.recent.len(), 1);
    }
}
