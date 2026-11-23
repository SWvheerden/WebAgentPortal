//! Spec files: the "Spawn from spec" picker's folder, and what a spec turns
//! into when an agent is spawned from it.
//!
//! A spec is a markdown file under the configured `spec_folder`. Spawning from
//! one names the agent `<folder>-<title>`, sends the whole file as the first
//! message wrapped in the `/new-feat` instruction, and records the file's path
//! on the agent so deleting the agent deletes the spec.
//!
//! Every path here is confined to the spec folder after canonicalisation, both
//! when a spec is chosen and again when it is deleted: the path stored on an
//! agent is never trusted to still be inside the folder on its own say-so.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Longest title carried into an agent's name, in characters.
pub const MAX_TITLE_CHARS: usize = 150;
/// The largest spec read into a prompt.
pub const MAX_SPEC_BYTES: u64 = 1024 * 1024;
/// How deep the picker looks below the spec folder.
const MAX_DEPTH: usize = 4;
/// Cap on how many specs the picker lists.
const MAX_ENTRIES: usize = 2000;
/// Extensions offered as specs. Everything else in the folder (zips, scripts)
/// is not a spec.
const EXTENSIONS: &[&str] = &["md", "markdown", "txt"];

/// One file the picker offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpecEntry {
    /// Path relative to the spec folder, `/`-separated. What the spawn sends
    /// back.
    pub path: String,
    /// The folder the file sits in, which prefixes the agent's name.
    pub folder: String,
    /// The spec's title, as it would appear in the agent's name.
    pub title: String,
    /// The agent name a spawn from this spec would get.
    pub agent_name: String,
}

/// A spec chosen for a spawn, read and resolved.
#[derive(Debug, Clone)]
pub struct ResolvedSpec {
    /// Canonical absolute path, inside the spec folder.
    pub path: PathBuf,
    pub agent_name: String,
    pub first_message: String,
}

/// The title: a `TITLE:` line or the first markdown heading, whichever comes
/// first; failing both, the file name without its extension. Cut to
/// [`MAX_TITLE_CHARS`] characters.
pub fn spec_title(contents: &str, file_stem: &str) -> String {
    let found = contents.lines().map(str::trim).find_map(|line| {
        if let Some(rest) = line.strip_prefix("TITLE:") {
            Some(rest.trim().to_string())
        } else if line.starts_with('#') {
            Some(line.trim_start_matches('#').trim().to_string())
        } else {
            None
        }
    });
    let title = found
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| file_stem.trim().to_string());
    title
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// `<folder>-<title>`.
pub fn agent_name(folder: &str, title: &str) -> String {
    format!("{folder}-{title}")
}

/// The first message an agent spawned from a spec is given.
pub fn first_message(contents: &str) -> String {
    format!(
        "use the new-feat skill to implement the following: {}\n\
         Use Opus model of the coder, reviewer and do a security review with opus as well.",
        contents.trim_end()
    )
}

/// The name of the folder `file` sits in.
fn folder_name(file: &Path) -> String {
    file.parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "spec".to_string())
}

fn is_spec_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Resolve `path` and require it to be a regular file strictly inside `dir`.
///
/// Both sides are canonicalised, so neither `..` nor a symlink pointing out of
/// the folder gets through.
pub fn confine(dir: &Path, path: &Path) -> Result<PathBuf> {
    let root =
        std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let resolved =
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    if resolved == root || !resolved.starts_with(&root) {
        bail!(
            "{} is outside the spec folder {}",
            resolved.display(),
            root.display()
        );
    }
    if !resolved.is_file() {
        bail!("{} is not a file", resolved.display());
    }
    Ok(resolved)
}

/// Read the spec at `relative` (relative to `dir`) and build the spawn from it.
///
/// Blocking: call from `spawn_blocking`.
pub fn resolve(dir: &Path, relative: &str) -> Result<ResolvedSpec> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        bail!("a spec path must be relative to the spec folder");
    }
    let path = confine(dir, &dir.join(rel))?;
    if !is_spec_file(&path) {
        bail!("{} is not a spec file", path.display());
    }
    let size = std::fs::metadata(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    if size > MAX_SPEC_BYTES {
        bail!(
            "{} is {size} bytes; specs over {MAX_SPEC_BYTES} bytes are refused",
            path.display()
        );
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if contents.trim().is_empty() {
        bail!("{} is empty", path.display());
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let title = spec_title(&contents, &stem);
    Ok(ResolvedSpec {
        agent_name: agent_name(&folder_name(&path), &title),
        first_message: first_message(&contents),
        path,
    })
}

/// Every spec under `dir`, sorted by path. Hidden files and folders are
/// skipped, and symlinks are not followed.
///
/// Blocking: call from `spawn_blocking`.
pub fn list(dir: &Path) -> Result<Vec<SpecEntry>> {
    let root =
        std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let mut out = Vec::new();
    walk(&root, &root, 0, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<SpecEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                walk(root, &path, depth + 1, out);
            }
        } else if kind.is_file() && is_spec_file(&path) {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let title = read_head(&path)
                .map(|head| spec_title(&head, &stem))
                .unwrap_or_else(|| stem.clone());
            let folder = folder_name(&path);
            out.push(SpecEntry {
                path: rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
                agent_name: agent_name(&folder, &title),
                folder,
                title,
            });
        }
    }
}

/// The first few KiB of a file, enough to find its title.
fn read_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0u8; 8 * 1024];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Delete the spec an agent was spawned from.
///
/// The stored path is re-confined to the *current* spec folder first: if the
/// folder was reconfigured, or the file replaced by a symlink pointing
/// elsewhere, nothing is deleted. A file that is already gone is not an error.
///
/// Blocking: call from `spawn_blocking`.
pub fn delete(dir: &Path, stored: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(stored) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", stored.display())),
    };
    if !meta.file_type().is_file() {
        bail!(
            "{} is no longer a regular file; left in place",
            stored.display()
        );
    }
    let path = confine(dir, stored)?;
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_comes_from_the_first_heading_or_title_line() {
        assert_eq!(
            spec_title("# SPEC-19: thing\n\nbody", "x"),
            "SPEC-19: thing"
        );
        assert_eq!(
            spec_title("TITLE: fix(mempool): bound\n\n# Spec: other", "x"),
            "fix(mempool): bound"
        );
        assert_eq!(spec_title("intro\n## Second level\n", "x"), "Second level");
        assert_eq!(spec_title("no heading here", "02-file"), "02-file");
        assert_eq!(spec_title("#\n", "stem"), "stem");
    }

    #[test]
    fn title_is_cut_to_150_characters() {
        let long = format!("# {}", "é".repeat(400));
        assert_eq!(spec_title(&long, "x").chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn the_first_message_wraps_the_whole_spec() {
        let msg = first_message("# Title\n\nline two\n");
        assert!(msg.starts_with(
            "use the new-feat skill to implement the following: # Title\n\nline two\n"
        ));
        assert!(msg.ends_with("do a security review with opus as well."));
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("specs");
        std::fs::create_dir_all(dir.join("cake")).expect("mkdir");
        std::fs::create_dir_all(dir.join(".hidden")).expect("mkdir");
        std::fs::write(dir.join("cake/19-pid.md"), "# SPEC-19: pid kill\nbody").expect("w");
        std::fs::write(dir.join("cake/notes.zip"), "zip").expect("w");
        std::fs::write(dir.join(".hidden/x.md"), "# hidden").expect("w");
        std::fs::write(dir.join("top.md"), "TITLE: top level\n").expect("w");
        std::fs::write(tmp.path().join("outside.md"), "# outside").expect("w");
        (tmp, dir)
    }

    #[test]
    fn listing_offers_spec_files_only() {
        let (_tmp, dir) = fixture();
        let specs = list(&dir).expect("list");
        let paths: Vec<_> = specs.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["cake/19-pid.md", "top.md"]);
        assert_eq!(specs[0].agent_name, "cake-SPEC-19: pid kill");
        assert_eq!(specs[1].agent_name, "specs-top level");
    }

    #[test]
    fn resolve_builds_the_spawn_and_confines_the_path() {
        let (_tmp, dir) = fixture();
        let spec = resolve(&dir, "cake/19-pid.md").expect("resolve");
        assert_eq!(spec.agent_name, "cake-SPEC-19: pid kill");
        assert!(spec.first_message.contains("body"));

        assert!(resolve(&dir, "../outside.md").is_err(), "dot-dot escape");
        let abs = dir.join("top.md").to_string_lossy().to_string();
        assert!(resolve(&dir, &abs).is_err(), "absolute path");
        assert!(resolve(&dir, "cake/notes.zip").is_err(), "not a spec");
        assert!(resolve(&dir, "cake").is_err(), "a folder");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                dir.parent().unwrap().join("outside.md"),
                dir.join("link.md"),
            )
            .expect("symlink");
            assert!(resolve(&dir, "link.md").is_err(), "symlink out");
        }
    }

    #[test]
    fn delete_removes_only_files_inside_the_folder() {
        let (tmp, dir) = fixture();
        let spec = resolve(&dir, "cake/19-pid.md").expect("resolve");
        delete(&dir, &spec.path).expect("delete");
        assert!(!spec.path.exists());
        // Already gone is fine.
        delete(&dir, &spec.path).expect("idempotent");

        let outside = tmp.path().join("outside.md");
        assert!(delete(&dir, &outside).is_err());
        assert!(
            outside.exists(),
            "a file outside the folder is never deleted"
        );

        // A spec replaced by a symlink is left alone, and so is its target.
        #[cfg(unix)]
        {
            let top = std::fs::canonicalize(dir.join("top.md")).expect("canon");
            std::fs::remove_file(&top).expect("rm");
            std::os::unix::fs::symlink(&outside, &top).expect("symlink");
            assert!(delete(&dir, &top).is_err());
            assert!(outside.exists());
        }
    }
}
