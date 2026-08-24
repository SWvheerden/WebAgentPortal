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
    /// This tool's own last-used timestamp, not the filesystem's.
    pub last_used_at: Option<i64>,
}

/// The picker payload: a Recent group and everything, alphabetically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoListing {
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

    order(entries, errors)
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
pub fn order(mut entries: Vec<RepoEntry>, errors: Vec<String>) -> RepoListing {
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
        );
        let names: Vec<_> = listing.all.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn recent_is_newest_first_and_capped() {
        let entries = (1..=8)
            .map(|i| entry(&format!("repo{i}"), Some(i as i64 * 100)))
            .collect();
        let listing = order(entries, vec![]);
        assert_eq!(listing.recent.len(), RECENT_LIMIT);
        let names: Vec<_> = listing.recent.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["repo8", "repo7", "repo6", "repo5", "repo4"]);
        assert_eq!(listing.all.len(), 8, "All still lists everything");
    }

    #[test]
    fn never_used_repos_are_absent_from_recent() {
        let listing = order(vec![entry("used", Some(5)), entry("never", None)], vec![]);
        assert_eq!(listing.recent.len(), 1);
        assert_eq!(listing.recent[0].name, "used");
    }

    #[test]
    fn recency_ties_break_alphabetically() {
        let listing = order(vec![entry("b", Some(10)), entry("a", Some(10))], vec![]);
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
