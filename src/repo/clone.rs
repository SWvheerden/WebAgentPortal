//! `git clone` with credential prompts disabled, streaming progress.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Derive a default folder name from a clone URL.
///
/// `https://host/owner/repo.git` and `git@host:owner/repo.git` both give `repo`.
pub fn folder_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let tail = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    if tail.is_empty() || tail.contains(['\\', '\0']) {
        return None;
    }
    Some(tail.to_string())
}

/// Where a clone will land, refusing anything that escapes the chosen root.
pub fn clone_destination(root: &Path, folder: &str) -> Result<PathBuf> {
    if folder.is_empty()
        || folder == "."
        || folder == ".."
        || folder.contains('/')
        || folder.contains('\\')
    {
        bail!("invalid folder name: {folder:?}");
    }
    Ok(root.join(folder))
}

/// Turn git's stderr into a friendlier message where we can.
pub fn hint_for_failure(stderr: &str) -> Option<&'static str> {
    let lower = stderr.to_lowercase();
    if lower.contains("authentication failed")
        || lower.contains("could not read username")
        || lower.contains("terminal prompts disabled")
        || lower.contains("permission denied (publickey)")
    {
        Some(
            "Authentication failed. Try an SSH URL (git@host:owner/repo.git) — this server never stores credentials and relies on your ssh-agent or git credential helper.",
        )
    } else if lower.contains("repository not found") || lower.contains("does not exist") {
        Some(
            "Repository not found. Check the URL, or whether the account you are authenticated as can see it.",
        )
    } else {
        None
    }
}

/// The outcome of a clone.
#[derive(Debug, Clone)]
pub struct CloneOutcome {
    pub path: PathBuf,
    pub stderr: String,
}

/// Clone `url` into `root/folder`, calling `on_progress` for each line git
/// writes to stderr.
///
/// `GIT_TERMINAL_PROMPT=0` and the askpass helpers are disabled so a private
/// repo fails fast instead of hanging on a credential prompt (§6).
pub async fn clone(
    url: &str,
    root: &Path,
    folder: &str,
    mut on_progress: impl FnMut(String),
) -> Result<CloneOutcome> {
    let dest = clone_destination(root, folder)?;
    if dest.exists() {
        bail!(
            "{} already exists — refusing to clone over it",
            dest.display()
        );
    }
    tokio::fs::create_dir_all(root)
        .await
        .with_context(|| format!("creating {}", root.display()))?;

    let mut child = Command::new("git")
        .arg("clone")
        .arg("--progress")
        .arg(url)
        .arg(&dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning git clone")?;

    let mut collected = String::new();
    if let Some(stderr) = child.stderr.take() {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .context("reading git clone output")?
        {
            collected.push_str(&line);
            collected.push('\n');
            on_progress(line);
        }
    }

    let status = child.wait().await.context("waiting for git clone")?;
    if !status.success() {
        // Do not leave a half-written directory behind.
        tokio::fs::remove_dir_all(&dest).await.ok();
        let hint = hint_for_failure(&collected)
            .map(|h| format!("\n\n{h}"))
            .unwrap_or_default();
        bail!("git clone failed: {}{hint}", collected.trim());
    }

    Ok(CloneOutcome {
        path: dest,
        stderr: collected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_come_from_the_url_tail() {
        assert_eq!(
            folder_name_from_url("https://github.com/owner/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            folder_name_from_url("https://github.com/owner/repo").as_deref(),
            Some("repo")
        );
        assert_eq!(
            folder_name_from_url("git@github.com:owner/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            folder_name_from_url("https://github.com/owner/repo/").as_deref(),
            Some("repo")
        );
        assert_eq!(
            folder_name_from_url("/local/path/thing").as_deref(),
            Some("thing")
        );
        assert_eq!(folder_name_from_url("   "), None);
    }

    #[test]
    fn clone_destination_rejects_traversal() {
        let root = Path::new("/root");
        assert_eq!(
            clone_destination(root, "repo").expect("ok"),
            PathBuf::from("/root/repo")
        );
        for bad in ["", ".", "..", "a/b", "../escape", "a\\b"] {
            assert!(
                clone_destination(root, bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn credential_failures_get_an_ssh_hint() {
        let hint = hint_for_failure("fatal: could not read Username for 'https://github.com'");
        assert!(hint.is_some_and(|h| h.contains("SSH URL")));
        assert!(hint_for_failure("some other failure").is_none());
        assert!(
            hint_for_failure("ERROR: Repository not found.")
                .is_some_and(|h| h.contains("not found"))
        );
    }

    #[tokio::test]
    async fn refuses_an_existing_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("taken")).expect("mkdir");
        let err = clone("https://example.invalid/x.git", dir.path(), "taken", |_| {})
            .await
            .expect_err("should refuse");
        assert!(err.to_string().contains("refusing to clone over it"));
    }

    #[tokio::test]
    async fn clones_a_local_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src-repo");
        std::fs::create_dir_all(&src).expect("mkdir");
        let ok = |args: &[&str]| super::super::git::git(&src, args).is_ok();
        if !ok(&["init", "-q", "-b", "main"]) {
            return; // no usable git binary
        }
        ok(&["config", "user.email", "t@example.com"]);
        ok(&["config", "user.name", "T"]);
        std::fs::write(src.join("f.txt"), "hi").expect("write");
        ok(&["add", "."]);
        if !ok(&["commit", "-q", "-m", "init"]) {
            return;
        }

        let dest_root = dir.path().join("dest");
        let mut progress = Vec::new();
        let outcome = clone(&src.to_string_lossy(), &dest_root, "cloned", |line| {
            progress.push(line)
        })
        .await
        .expect("clone");
        assert!(outcome.path.join("f.txt").exists());
        assert!(!progress.is_empty(), "clone should stream progress lines");
    }
}
