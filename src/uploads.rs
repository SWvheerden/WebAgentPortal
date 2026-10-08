//! Files the operator attaches to a message (§7, "Attaching files").
//!
//! Each agent gets one folder, `~/.claude-web/uploads/<agent-id>/`, outside
//! every repository and worktree. The agent is launched with `--add-dir` on it
//! and is handed absolute paths in the message; what it does with the files is
//! its own business. Deleting the agent wipes the folder.
//!
//! The folder is writable by the agent, so nothing here trusts what is in it:
//! uploads are created with `create_new` (a planted symlink makes the write
//! fail rather than follow it), downloads refuse anything that is not a plain
//! file, and the wipe never follows a link.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// The longest file name we store, in bytes.
pub const MAX_NAME_BYTES: usize = 120;

/// The name used when cleaning leaves nothing.
const FALLBACK_NAME: &str = "upload";

/// An attachment as it travels with a message: stored on the user event and
/// listed in the trailer the agent reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub size: u64,
    /// Absolute path on the server. Built by the server, never by a client.
    pub path: String,
}

/// Turn a browser-supplied file name into one that is safe to store and easy
/// to type in a shell.
///
/// Basename only; control characters dropped; anything that is not a letter,
/// a digit, `.`, `_` or `-` becomes `-` (runs collapse to one); no leading dot
/// or dash, so it is neither hidden nor option-shaped; at most
/// [`MAX_NAME_BYTES`], keeping the extension; `upload` if nothing is left.
pub fn clean_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::new();
    // NFC first: a Mac hands over `café` decomposed, and its combining accent
    // is not a letter, so it would otherwise become `cafe-`. The name on disk
    // and the row then agree on one spelling.
    for c in base.nfc() {
        if c.is_control() {
            continue;
        }
        let keep = c.is_alphanumeric() || matches!(c, '.' | '_' | '-');
        let c = if keep { c } else { '-' };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_start_matches(['.', '-']);
    if out.is_empty() {
        return FALLBACK_NAME.to_string();
    }
    let (stem, ext) = split_ext(out);
    fit(stem, ext, "").nfc().collect()
}

/// The key two names collide on. The default macOS filesystem treats `A.txt`
/// and `a.txt`, or NFC and NFD `café`, as one file, so uniqueness in the
/// `uploads` table is on this rather than the exact name — otherwise a new
/// upload could land on an old row's file under a different spelling.
pub fn fold_key(name: &str) -> String {
    let lower: String = name.nfc().collect::<String>().to_lowercase();
    lower.nfc().collect()
}

/// `name` with a numeric suffix before its extension: `report.pdf` and 2 give
/// `report-2.pdf`. Still at most [`MAX_NAME_BYTES`].
pub fn with_suffix(name: &str, n: u32) -> String {
    let (stem, ext) = split_ext(name);
    fit(stem, ext, &format!("-{n}"))
}

/// Split off a short extension. A leading dot never counts — `clean_name` has
/// removed those — and neither does a long tail, which is more likely part of
/// the name than a file type.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 16 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    }
}

/// `stem + suffix + ext`, with the stem cut (on a character boundary) so the
/// whole fits in [`MAX_NAME_BYTES`].
///
/// Every name this returns is a fixed point of [`clean_name`]: the routes look
/// a stored name up by checking it cleans to itself. So a stem ending in `-`
/// loses it before a `-N` suffix goes on — `Screenshot-1-` and 2 must give
/// `Screenshot-1-2`, not a `--` that cleaning would collapse.
fn fit(stem: &str, ext: &str, suffix: &str) -> String {
    let room = MAX_NAME_BYTES.saturating_sub(ext.len() + suffix.len());
    let mut cut = stem.len().min(room);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut stem = &stem[..cut];
    if !suffix.is_empty() {
        stem = stem.trim_end_matches('-');
    }
    let stem = if stem.is_empty() { FALLBACK_NAME } else { stem };
    format!("{stem}{suffix}{ext}")
}

/// Create a new file in `dir` named `name`, or `name-2`, `name-3`… if that is
/// taken, trying suffixes from `first` up (1 meaning the bare name). Never
/// overwrites, and never follows a symlink someone left at the name:
/// `create_new` fails on any existing entry, links included.
///
/// Returns the open file, the name it got, and that name's suffix number, so
/// a caller that finds the name taken elsewhere can carry on past it.
///
/// No practical cap: callers start past every suffix already recorded (see
/// [`next_suffix`]), so the walk only ever steps over files the agent made.
pub fn create_unique(dir: &Path, name: &str, first: u32) -> Result<(File, String, u32)> {
    for n in first.max(1)..=u32::MAX {
        let candidate = if n == 1 {
            name.to_string()
        } else {
            with_suffix(name, n)
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&candidate))
        {
            Ok(file) => return Ok((file, candidate, n)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("creating {}", dir.join(&candidate).display()));
            }
        }
    }
    bail!("too many files called {name} already")
}

/// The first suffix worth trying for `wanted`: one past the highest already
/// recorded for the same stem and extension (1, the bare name, if none).
///
/// `folds` are the agent's recorded [`fold_key`]s, so `IMAGE-7.PNG` counts
/// against `image.png`. Without this, every pasted `image.png` would walk
/// every earlier one, on disk and in the table, before finding a free name.
/// A name `with_suffix` had to shorten to fit is not recognised; it only costs
/// a few extra steps in [`create_unique`].
pub fn next_suffix(wanted: &str, folds: &[String]) -> u32 {
    let wanted = fold_key(wanted);
    let (stem, ext) = split_ext(&wanted);
    let prefix = format!("{}-", stem.trim_end_matches('-'));
    let mut highest: u32 = 0;
    for key in folds {
        let (s, e) = split_ext(key);
        if e != ext {
            continue;
        }
        if s == stem {
            highest = highest.max(1);
            continue;
        }
        let Some(rest) = s.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = rest.parse::<u32>() {
            highest = highest.max(n);
        }
    }
    highest.saturating_add(1)
}

/// Make sure an agent's upload folder exists and is a real directory, not a
/// symlink pointing somewhere else.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        bail!("{} is not a plain directory", dir.display());
    }
    Ok(())
}

/// Open an uploaded file for download, refusing anything that is not a plain
/// file. Returns the open file and its length at the moment it was opened.
///
/// The agent can write in this folder, so a name we stored may since have
/// been replaced by a link to `~/.ssh/id_ed25519`. The entry is checked with
/// `symlink_metadata`, then opened, and the opened file must be the same
/// inode — so a swap between the check and the open is refused too.
///
/// The caller streams it rather than reading it whole: the agent can grow the
/// file in place past the upload cap, and a download must not pull that into
/// memory.
pub fn open_plain_file(dir: &Path, name: &str) -> Result<(File, u64)> {
    use std::os::unix::fs::MetadataExt;

    let dir_meta = std::fs::symlink_metadata(dir)?;
    if !dir_meta.file_type().is_dir() {
        bail!("the upload folder is not a plain directory");
    }
    let path = dir.join(name);
    let link = std::fs::symlink_metadata(&path)?;
    if !link.file_type().is_file() {
        bail!("{name} is not a plain file");
    }
    let file = File::open(&path)?;
    let opened = file.metadata()?;
    if opened.ino() != link.ino() || opened.dev() != link.dev() {
        bail!("{name} changed while it was being opened");
    }
    Ok((file, opened.len()))
}

/// Remove an agent's whole upload folder. A missing folder is fine. A symlink
/// where the folder should be is removed as a link; `remove_dir_all` itself
/// never follows the links inside.
pub fn wipe_dir(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("inspecting {}", dir.display())),
        Ok(meta) if meta.file_type().is_dir() => {
            std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
        }
        Ok(_) => std::fs::remove_file(dir).with_context(|| format!("removing {}", dir.display())),
    }
}

/// A byte count the way a person reads it: `512 B`, `1.5 KB`, `3.2 MB`.
/// Mirrored by `humanSize` in uploads.js; a test holds them together.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The text the CLI is actually sent: what was typed, then one line per file.
///
/// ```text
/// <text>
///
/// Attached files:
/// - /home/me/.claude-web/uploads/<id>/report.pdf (1.2 MB)
/// ```
pub fn with_trailer(text: &str, files: &[Attachment]) -> String {
    if files.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    if !text.is_empty() {
        out.push_str(text);
        out.push_str("\n\n");
    }
    out.push_str("Attached files:");
    for file in files {
        out.push_str(&format!("\n- {} ({})", file.path, human_size(file.size)));
    }
    out
}

/// `filename*=` wants RFC 5987 percent-encoding; everything outside the
/// unreserved set is escaped byte by byte.
pub fn percent_encode(name: &str) -> String {
    let mut out = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `Content-Disposition` for a download: always an attachment, never
/// rendered inline. The plain `filename=` gets an ASCII stand-in; browsers
/// that understand `filename*=` use the real name.
pub fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        percent_encode(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_reduced_to_a_safe_basename() {
        assert_eq!(clean_name("report.pdf"), "report.pdf");
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Users\\me\\notes.txt"), "notes.txt");
        assert_eq!(clean_name(".bashrc"), "bashrc");
        assert_eq!(clean_name("..."), "upload");
        assert_eq!(clean_name(""), "upload");
        assert_eq!(clean_name("dir/"), "upload");
        assert_eq!(clean_name("-rf"), "rf");
        assert_eq!(clean_name("my report (1).pdf"), "my-report-1-.pdf");
        assert_eq!(clean_name("a\u{0}b\nc\td.txt"), "abcd.txt");
        assert_eq!(clean_name("$(rm -rf ~)`x`;'q'\".sh"), "rm-rf-x-q-.sh");
        // Letters in any script stay.
        assert_eq!(clean_name("résumé 日本.txt"), "résumé-日本.txt");
        // A decomposed name comes out composed, accent intact.
        let nfd = "cafe\u{301}.txt";
        assert_eq!(clean_name(nfd), "caf\u{e9}.txt");
        assert_eq!(clean_name(nfd), clean_name("caf\u{e9}.txt"));
        assert_eq!(clean_name(&clean_name(nfd)), clean_name(nfd));
    }

    #[test]
    fn names_that_one_file_on_macos_would_share_fold_to_one_key() {
        assert_eq!(fold_key("Report.PDF"), fold_key("report.pdf"));
        assert_eq!(fold_key("cafe\u{301}.txt"), fold_key("CAF\u{c9}.txt"));
        assert_ne!(fold_key("report.pdf"), fold_key("report-2.pdf"));
    }

    #[test]
    fn long_names_are_capped_and_keep_their_extension() {
        let long = format!("{}.pdf", "a".repeat(300));
        let cleaned = clean_name(&long);
        assert_eq!(cleaned.len(), MAX_NAME_BYTES);
        assert!(cleaned.ends_with(".pdf"));

        // A multi-byte character is never split.
        let wide = format!("{}.txt", "é".repeat(100));
        let cleaned = clean_name(&wide);
        assert!(cleaned.len() <= MAX_NAME_BYTES);
        assert!(cleaned.ends_with(".txt"));

        let suffixed = with_suffix(&clean_name(&long), 12);
        assert_eq!(suffixed.len(), MAX_NAME_BYTES);
        assert!(suffixed.ends_with("-12.pdf"), "{suffixed}");
    }

    /// The routes find a stored name by checking it cleans to itself, so
    /// every generated name must — a `--` from a stem ending in `-` would
    /// collapse, and that upload could never be downloaded or withdrawn.
    #[test]
    fn every_generated_name_is_a_fixed_point_of_cleaning() {
        let ends_in_dash = clean_name("Screenshot (1).png");
        assert_eq!(ends_in_dash, "Screenshot-1-.png");
        assert_eq!(with_suffix(&ends_in_dash, 2), "Screenshot-1-2.png");
        assert_eq!(with_suffix("a-", 2), "a-2");
        assert_eq!(with_suffix("-.txt", 2), "upload-2.txt");

        // Truncation that lands right after a `-`.
        let long_dashed = format!("{}-{}.pdf", "a".repeat(115), "b".repeat(50));
        let mut names = vec![
            ends_in_dash.clone(),
            clean_name(&long_dashed),
            clean_name(&format!("{}.pdf", "a-".repeat(80))),
            clean_name(&format!("{}.txt", "é-".repeat(60))),
            clean_name("plain"),
        ];
        for base in names.clone() {
            for n in [2, 9, 10, 99, 999] {
                names.push(with_suffix(&base, n));
            }
        }
        for name in names {
            assert_eq!(clean_name(&name), name, "{name} does not clean to itself");
            assert!(name.len() <= MAX_NAME_BYTES, "{name}");
        }
    }

    #[test]
    fn probing_starts_past_the_highest_recorded_suffix() {
        let folds = |names: &[&str]| names.iter().map(|n| fold_key(n)).collect::<Vec<_>>();
        assert_eq!(next_suffix("image.png", &[]), 1);
        assert_eq!(next_suffix("image.png", &folds(&["image.png"])), 2);
        assert_eq!(
            next_suffix(
                "image.png",
                &folds(&["image.png", "IMAGE-7.PNG", "image-3.png"])
            ),
            8,
            "case variants count"
        );
        assert_eq!(
            next_suffix(
                "image.png",
                &folds(&[
                    "image-copy.png",
                    "image-2x.png",
                    "image-5.jpg",
                    "other-9.png",
                    "image-.png"
                ])
            ),
            1,
            "other stems and extensions do not"
        );
        // A stem ending in `-` is numbered the way `with_suffix` numbers it.
        assert_eq!(
            next_suffix("a-1-.pdf", &folds(&["a-1-.pdf", "a-1-4.pdf"])),
            5
        );
        assert_eq!(
            next_suffix("Makefile", &folds(&["makefile", "Makefile-2"])),
            3
        );
        assert_eq!(
            next_suffix("x.txt", &folds(&["x-99999999999.txt"])),
            1,
            "overflow is ignored"
        );
    }

    #[test]
    fn a_suffix_goes_before_the_extension() {
        assert_eq!(with_suffix("report.pdf", 2), "report-2.pdf");
        assert_eq!(with_suffix("Makefile", 3), "Makefile-3");
        assert_eq!(with_suffix("archive.tar.gz", 2), "archive.tar-2.gz");
    }

    #[test]
    fn collisions_get_a_numbered_name_and_nothing_is_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, first, _) = create_unique(dir.path(), "report.pdf", 1).expect("first");
        let (_, second, _) = create_unique(dir.path(), "report.pdf", 1).expect("second");
        let (_, third, n) = create_unique(dir.path(), "report.pdf", 1).expect("third");
        assert_eq!(n, 3);
        let (_, later, _) = create_unique(dir.path(), "report.pdf", 7).expect("later");
        assert_eq!(later, "report-7.pdf", "starting past a name skips it");
        assert_eq!(
            (first.as_str(), second.as_str(), third.as_str()),
            ("report.pdf", "report-2.pdf", "report-3.pdf")
        );
    }

    #[test]
    fn a_planted_symlink_is_never_written_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "precious").expect("write");
        let uploads = dir.path().join("uploads");
        std::fs::create_dir(&uploads).expect("mkdir");
        std::os::unix::fs::symlink(&target, uploads.join("notes.txt")).expect("symlink");

        let (_, name, _) = create_unique(&uploads, "notes.txt", 1).expect("create");
        assert_eq!(
            name, "notes-2.txt",
            "the link's name is skipped, not followed"
        );
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "precious");
    }

    #[test]
    fn a_symlink_is_refused_on_download() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "key").expect("write");
        let uploads = dir.path().join("uploads");
        std::fs::create_dir(&uploads).expect("mkdir");
        std::fs::write(uploads.join("plain.txt"), "hello").expect("write");
        std::os::unix::fs::symlink(&secret, uploads.join("link.txt")).expect("symlink");

        let (mut file, len) = open_plain_file(&uploads, "plain.txt").expect("plain");
        let mut data = String::new();
        std::io::Read::read_to_string(&mut file, &mut data).expect("read");
        assert_eq!((data.as_str(), len), ("hello", 5));
        assert!(open_plain_file(&uploads, "link.txt").is_err());

        // A folder that has been swapped for a link is refused as a whole.
        let other = dir.path().join("other");
        std::fs::create_dir(&other).expect("mkdir");
        std::fs::write(other.join("plain.txt"), "elsewhere").expect("write");
        let swapped = dir.path().join("swapped");
        std::os::unix::fs::symlink(&other, &swapped).expect("symlink");
        assert!(open_plain_file(&swapped, "plain.txt").is_err());
    }

    #[test]
    fn the_wipe_does_not_follow_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::write(outside.join("keep.txt"), "keep").expect("write");

        let uploads = dir.path().join("uploads");
        std::fs::create_dir(&uploads).expect("mkdir");
        std::fs::write(uploads.join("a.txt"), "a").expect("write");
        std::os::unix::fs::symlink(&outside, uploads.join("escape")).expect("symlink");
        wipe_dir(&uploads).expect("wipe");
        assert!(!uploads.exists());
        assert!(
            outside.join("keep.txt").exists(),
            "a link inside is not followed"
        );

        // The folder itself being a link: the link goes, the target stays.
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&outside, &linked).expect("symlink");
        wipe_dir(&linked).expect("wipe");
        assert!(std::fs::symlink_metadata(&linked).is_err());
        assert!(outside.join("keep.txt").exists());

        // Nothing there is not an error.
        wipe_dir(&dir.path().join("never-made")).expect("missing is fine");
    }

    #[test]
    fn sizes_read_like_a_person_would_say_them() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn the_trailer_lists_every_file_by_absolute_path() {
        let files = vec![
            Attachment {
                name: "a.png".into(),
                size: 2048,
                path: "/u/a.png".into(),
            },
            Attachment {
                name: "b.txt".into(),
                size: 10,
                path: "/u/b.txt".into(),
            },
        ];
        assert_eq!(
            with_trailer("look at these", &files),
            "look at these\n\nAttached files:\n- /u/a.png (2.0 KB)\n- /u/b.txt (10 B)"
        );
        assert_eq!(
            with_trailer("", &files[..1]),
            "Attached files:\n- /u/a.png (2.0 KB)"
        );
        assert_eq!(with_trailer("plain", &[]), "plain");
    }

    #[test]
    fn a_download_is_always_an_attachment() {
        assert_eq!(
            content_disposition("résumé.pdf"),
            "attachment; filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"
        );
    }
}
