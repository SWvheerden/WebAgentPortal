//! One hardened walker for trees an agent controls: its upload folder, its
//! worktree, a clone that failed half-way. Counting them and removing them go
//! through [`walk_tree`]; nothing else in the server touches such a tree by
//! path or with `std::fs::remove_dir_all`, whose recursion an agent can
//! overflow with a deep enough tree (DESIGN.md §7, "One walker").

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

/// How deep [`walk_tree`] goes below the folder it starts in. Deeper than
/// this, the walk fails closed.
///
/// It also bounds the walk's open files. A walk holds one descriptor per
/// level on its stack — at most `WALK_MAX_DEPTH + 1` with the top — plus one
/// for the moment it lists a folder and, in [`remove_tree`], one for the
/// portal-owned parent: about 131. Only one walk runs at a time
/// ([`WALK_LOCK`]), so that is the whole cost, and it leaves the rest of
/// macOS's default soft limit of 256 to the server's sockets, database and
/// agents' pipes.
pub(crate) const WALK_MAX_DEPTH: usize = 128;

/// How many entries removing a checkout may visit: a worktree with a build
/// folder or `node_modules` holds far more than an upload folder, and the
/// removal is bounded by the disk anyway. The depth bound still applies.
pub const CHECKOUT_MAX_ENTRIES: usize = 20_000_000;

/// Held for the length of every walk: two concurrent walks would double the
/// open-file cost above, and a few would exhaust the limit.
static WALK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Walks in progress and the most ever seen at once, so a test can see that
/// they never overlap.
#[cfg(test)]
pub(crate) static WALKS_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static WALKS_MOST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// What a walk of an agent's folder found, or removed.
#[derive(Debug, Default, PartialEq)]
pub struct Walked {
    /// Everything that is not a folder: files, links, anything else.
    pub count: u64,
    /// The size of the regular files among them.
    pub bytes: u64,
    /// The whole tree was walked. When false, `count` and `bytes` are lower
    /// bounds and `problem` says why.
    pub complete: bool,
    pub problem: Option<String>,
}

impl Walked {
    pub(crate) fn file(&mut self, size: u64) {
        self.count = self.count.saturating_add(1);
        self.bytes = self.bytes.saturating_add(size);
    }

    fn fail(&mut self, problem: String) {
        self.complete = false;
        self.problem = Some(problem);
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum WalkMode {
    Count,
    Remove,
}

/// One open folder on the walk's stack, with the entries still to visit.
struct Frame {
    fd: OwnedFd,
    entries: Vec<CString>,
    /// Its name in the folder below it on the stack; `None` for the top.
    name: Option<CString>,
}

/// Open the folder `name` inside `parent`, refusing to follow a link.
pub(crate) fn open_dir_at<P: rustix::path::Arg, Fd: AsFd>(
    parent: Fd,
    name: P,
) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// The names in an open folder, without `.` and `..`.
///
/// Reads at most `limit + 1` names: a folder holding more than the walk has
/// left to spend is not read to the end — or held in memory — just to find
/// that out. The caller sees `limit + 1` names and fails closed. What is read
/// is sorted, so a walk visits entries in the same order every time.
pub(crate) fn list_dir(fd: &OwnedFd, limit: usize) -> rustix::io::Result<Vec<CString>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(fd)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            names.push(name.to_owned());
            if names.len() > limit {
                break;
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Open the folder `name` inside `parent` to remove it. If it cannot be
/// opened at all (mode 000, say), it gets one `fchmodat(AT_SYMLINK_NOFOLLOW)`
/// to 0700 from its parent and one more try — never a path, never a link
/// followed. Linux refuses that call, so there such a folder stays shut and
/// the caller fails closed.
fn open_dir_to_remove<Fd: AsFd>(parent: Fd, name: &CStr) -> rustix::io::Result<OwnedFd> {
    let parent = parent.as_fd();
    match open_dir_at(parent, name) {
        Err(Errno::ACCESS) => {
            rustix::fs::chmodat(
                parent,
                name,
                Mode::from_raw_mode(0o700),
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
            open_dir_at(parent, name)
        }
        other => other,
    }
}

/// Walk a tree the agent can write to, counting it or removing it.
///
/// The agent controls every name and every entry here, so the walk trusts no
/// path: each folder is opened relative to the folder it was found in, with
/// `O_DIRECTORY | O_NOFOLLOW`, so a folder swapped for a link is never entered;
/// entries are listed from the open folder; files and links are removed with
/// `unlinkat` relative to it, and folders with `unlinkat(AT_REMOVEDIR)` once
/// they are empty. Permissions are fixed (when removing) with `fchmod` on the
/// open folder, to 0700 — never by path, and never to a mode derived from
/// what the agent set. A folder that cannot even be opened gets one
/// `fchmodat(AT_SYMLINK_NOFOLLOW)` from its parent and one more try; where the
/// platform cannot change a mode without following a link (Linux), that fails
/// and the walk fails closed.
///
/// The walk is iterative, on a heap stack, so no tree can exhaust the thread's
/// stack; and it is bounded — [`WALK_MAX_DEPTH`] levels and `max_entries`
/// entries — failing closed beyond either. It stays on one filesystem: a
/// folder on a different device from the top (something mounted inside the
/// tree) is never entered, let alone emptied — the walk fails closed there
/// too. `top` is the open folder to start in; names in `skip_top` are passed
/// over at its level only. One walk runs at a time ([`WALK_LOCK`]).
pub(crate) fn walk_tree(
    top: OwnedFd,
    skip_top: &HashSet<String>,
    mode: WalkMode,
    max_entries: usize,
) -> Walked {
    let _one_at_a_time = WALK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(test)]
    let _active = ActiveWalk::start();
    let root_dev = match rustix::fs::fstat(&top) {
        Ok(stat) => stat.st_dev,
        Err(err) => {
            let mut walked = Walked::default();
            walked.fail(format!("could not inspect the folder: {err}"));
            return walked;
        }
    };
    walk_on_device(top, root_dev, skip_top, mode, max_entries)
}

/// Counts a walk in [`WALKS_ACTIVE`] for as long as it lasts.
#[cfg(test)]
struct ActiveWalk;

#[cfg(test)]
impl ActiveWalk {
    fn start() -> Self {
        use std::sync::atomic::Ordering;
        let now = WALKS_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        WALKS_MOST.fetch_max(now, Ordering::SeqCst);
        ActiveWalk
    }
}

#[cfg(test)]
impl Drop for ActiveWalk {
    fn drop(&mut self) {
        WALKS_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// [`walk_tree`] with the device the walk must stay on given explicitly —
/// the top's own, except in a test that needs to see a mount without making
/// one.
pub(crate) fn walk_on_device(
    top: OwnedFd,
    root_dev: rustix::fs::Dev,
    skip_top: &HashSet<String>,
    mode: WalkMode,
    max_entries: usize,
) -> Walked {
    let mut walked = Walked {
        complete: true,
        ..Walked::default()
    };
    let too_many = format!("more than {max_entries} entries");
    // The names passed over at the top are read too, so they get room.
    let top_limit = max_entries.saturating_add(skip_top.len());
    let entries: Vec<CString> = match list_dir(&top, top_limit) {
        Ok(entries) if entries.len() > top_limit => {
            walked.fail(too_many);
            return walked;
        }
        Ok(entries) => entries
            .into_iter()
            .filter(|n| !skip_top.contains(n.to_string_lossy().as_ref()))
            .collect(),
        Err(err) => {
            walked.fail(format!("could not list the folder: {err}"));
            return walked;
        }
    };
    if entries.len() > max_entries {
        walked.fail(too_many);
        return walked;
    }
    let mut stack = vec![Frame {
        fd: top,
        entries,
        name: None,
    }];
    let mut seen = 0usize;
    while let Some(index) = stack.len().checked_sub(1) {
        let Some(name) = stack[index].entries.pop() else {
            // This folder is done. When removing, it is now empty: take it
            // out of the folder below it.
            let done = stack.pop().expect("the stack is not empty");
            if mode == WalkMode::Remove
                && let (Some(name), Some(parent)) = (done.name, stack.last())
            {
                drop(done.fd);
                if let Err(err) = rustix::fs::unlinkat(&parent.fd, &name, AtFlags::REMOVEDIR) {
                    walked.fail(format!(
                        "could not remove {}: {err}",
                        name.to_string_lossy()
                    ));
                    return walked;
                }
            }
            continue;
        };
        if seen >= max_entries {
            walked.fail(format!("more than {max_entries} entries"));
            return walked;
        }
        seen += 1;
        let parent = &stack[index].fd;
        let stat = match rustix::fs::statat(parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(err) => {
                walked.fail(format!(
                    "could not inspect {}: {err}",
                    name.to_string_lossy()
                ));
                return walked;
            }
        };
        let kind = FileType::from_raw_mode(stat.st_mode as _);
        if kind == FileType::Directory {
            if stat.st_dev != root_dev {
                walked.fail(format!(
                    "a different filesystem is mounted at {}",
                    name.to_string_lossy()
                ));
                return walked;
            }
            if stack.len() > WALK_MAX_DEPTH {
                walked.fail(format!("folders nested more than {WALK_MAX_DEPTH} deep"));
                return walked;
            }
            let opened = match mode {
                WalkMode::Remove => open_dir_to_remove(parent, &name),
                WalkMode::Count => open_dir_at(parent, &name),
            };
            let fd = match opened {
                Ok(fd) => fd,
                // Swapped for a link (or a file) since it was inspected: it is
                // not a folder any more, and is never entered.
                Err(Errno::LOOP | Errno::NOTDIR) => {
                    walked.file(0);
                    if mode == WalkMode::Remove
                        && let Err(err) = rustix::fs::unlinkat(parent, &name, AtFlags::empty())
                    {
                        walked.fail(format!(
                            "could not remove {}: {err}",
                            name.to_string_lossy()
                        ));
                        return walked;
                    }
                    continue;
                }
                Err(err) => {
                    walked.fail(format!("could not open {}: {err}", name.to_string_lossy()));
                    return walked;
                }
            };
            if mode == WalkMode::Remove
                && let Err(err) = rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))
            {
                walked.fail(format!(
                    "could not unlock {}: {err}",
                    name.to_string_lossy()
                ));
                return walked;
            }
            // What is left of the budget, counting what is already queued.
            let queued: usize = stack.iter().map(|f| f.entries.len()).sum();
            let limit = max_entries.saturating_sub(seen).saturating_sub(queued);
            let entries = match list_dir(&fd, limit) {
                Ok(entries) if entries.len() > limit => {
                    walked.fail(too_many);
                    return walked;
                }
                Ok(entries) => entries,
                Err(err) => {
                    walked.fail(format!("could not list {}: {err}", name.to_string_lossy()));
                    return walked;
                }
            };
            stack.push(Frame {
                fd,
                entries,
                name: Some(name),
            });
            continue;
        }
        let size = if kind == FileType::RegularFile {
            u64::try_from(stat.st_size).unwrap_or(0)
        } else {
            0
        };
        walked.file(size);
        if mode == WalkMode::Remove
            && let Err(err) = rustix::fs::unlinkat(parent, &name, AtFlags::empty())
        {
            walked.fail(format!(
                "could not remove {}: {err}",
                name.to_string_lossy()
            ));
            return walked;
        }
    }
    walked
}

/// Count what is in a tree the agent controls: everything that is not a
/// folder, and the size of the regular files. Names in `skip_top` are passed
/// over at the top level only. A missing folder holds nothing; one that cannot
/// be walked to the end says so (`complete: false`).
pub fn count_tree(dir: &Path, skip_top: &HashSet<String>, cap: usize) -> Walked {
    match open_dir_at(rustix::fs::CWD, dir) {
        Ok(top) => walk_tree(top, skip_top, WalkMode::Count, cap),
        Err(Errno::NOENT) => Walked {
            complete: true,
            ..Walked::default()
        },
        Err(err) => {
            let mut walked = Walked::default();
            walked.fail(format!("could not open {}: {err}", dir.display()));
            walked
        }
    }
}

/// Remove a tree the agent controls — its upload folder, the private one, a
/// worktree git would not remove, a clone that failed. A missing one is fine;
/// a link where the folder should be is removed as a link. The tree
/// is removed by [`walk_tree`]: never following a link, getting through
/// read-only folders the agent left (a Go module cache is 0555), and failing
/// closed — leaving the folder, with an error — if it is deeper or bigger
/// than the walk will go.
///
/// Everything happens relative to the folder `dir` sits in (`uploads/`,
/// `blobs/`, `.worktrees/<repo>/`, a repo root): the entry is inspected,
/// opened (with the same unlock-and-retry as any folder inside it, so an
/// agent that made its own folder mode 000 does not keep it) and finally
/// removed with `unlinkat` there, never by path. `max_entries` bounds the
/// work; past it, as past the depth bound, the tree is left and the error
/// says so.
pub fn remove_tree(dir: &Path, max_entries: usize) -> Result<()> {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        bail!("{} is not a folder that can be removed", dir.display());
    };
    let name = CString::new(name.as_encoded_bytes())
        .with_context(|| format!("{} has a NUL in its name", dir.display()))?;
    let parent_fd = match open_dir_at(rustix::fs::CWD, parent) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("opening {}", parent.display())),
    };
    let stat = match rustix::fs::statat(&parent_fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("inspecting {}", dir.display())),
    };
    if FileType::from_raw_mode(stat.st_mode as _) != FileType::Directory {
        // A link (or a file) where the folder should be: removed as itself.
        return rustix::fs::unlinkat(&parent_fd, &name, AtFlags::empty())
            .with_context(|| format!("removing {}", dir.display()));
    }
    let top = open_dir_to_remove(&parent_fd, &name)
        .with_context(|| format!("opening {}", dir.display()))?;
    rustix::fs::fchmod(&top, Mode::from_raw_mode(0o700))
        .with_context(|| format!("unlocking {}", dir.display()))?;
    let walked = walk_tree(top, &HashSet::new(), WalkMode::Remove, max_entries);
    if let Some(problem) = walked.problem {
        bail!("could not remove {}: {problem}", dir.display());
    }
    rustix::fs::unlinkat(&parent_fd, &name, AtFlags::REMOVEDIR)
        .with_context(|| format!("removing {}", dir.display()))
}

/// Helpers for tests that need a tree deeper than any path can name.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Build `levels` nested folders under `dir`, far past any path-length
    /// limit, which is the point. Built inside out with short relative names:
    /// at each step a new folder is made beside the chain and the chain is
    /// moved into it, so no step ever names a deep path (and APFS, which
    /// slows down sharply making folders deep down, never has to).
    pub(crate) fn deep_tree(dir: &Path, levels: usize) {
        let top = open_dir_at(rustix::fs::CWD, dir).expect("open");
        rustix::fs::mkdirat(&top, "d", Mode::from_raw_mode(0o755)).expect("mkdirat");
        let leaf = open_dir_at(&top, "d").expect("open");
        rustix::fs::openat(
            &leaf,
            "leaf",
            OFlags::CREATE | OFlags::WRONLY,
            Mode::from_raw_mode(0o600),
        )
        .expect("leaf");
        for _ in 1..levels {
            rustix::fs::mkdirat(&top, "n", Mode::from_raw_mode(0o755)).expect("mkdirat");
            rustix::fs::renameat(&top, "d", &top, "n/d").expect("nest");
            rustix::fs::renameat(&top, "n", &top, "d").expect("rename");
        }
    }

    /// Take a deep tree apart without recursion or long paths, for cleanup:
    /// move the folder two levels down up to the top, remove the one it was
    /// in, and repeat.
    pub(crate) fn flatten_away(dir: &Path) {
        let top = open_dir_at(rustix::fs::CWD, dir).expect("open");
        while open_dir_at(&top, "d/d").is_ok() {
            rustix::fs::renameat(&top, "d/d", &top, "t").expect("lift");
            if let Ok(d) = open_dir_at(&top, "d") {
                for name in list_dir(&d, usize::MAX).expect("list") {
                    rustix::fs::unlinkat(&d, &name, AtFlags::empty()).ok();
                }
            }
            rustix::fs::unlinkat(&top, "d", AtFlags::REMOVEDIR).expect("rmdir");
            rustix::fs::renameat(&top, "t", &top, "d").expect("lower");
        }
        std::fs::remove_dir_all(dir).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder on another device — something mounted inside the tree — is
    /// never entered or emptied: the walk fails closed there. Making a real
    /// mount needs privileges a test does not have, so the device the walk
    /// must stay on is given as one the tree's folders are not on, which is
    /// exactly what a mount point looks like to it.
    #[test]
    fn a_walk_never_crosses_onto_another_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(agent.join("mounted")).expect("mkdir");
        std::fs::write(agent.join("mounted").join("theirs.txt"), "not ours").expect("write");
        std::fs::write(agent.join("a.txt"), "a").expect("write");

        let top = open_dir_at(rustix::fs::CWD, &agent).expect("open");
        let real = rustix::fs::fstat(&top).expect("stat").st_dev;
        let walked = walk_on_device(
            top,
            real.wrapping_add(1),
            &HashSet::new(),
            WalkMode::Remove,
            100,
        );
        assert_eq!(
            walked.problem.as_deref(),
            Some("a different filesystem is mounted at mounted")
        );
        assert!(
            agent.join("mounted").join("theirs.txt").exists(),
            "never emptied"
        );

        // On its own device the same tree is walked as usual.
        let top = open_dir_at(rustix::fs::CWD, &agent).expect("open");
        assert!(walk_tree(top, &HashSet::new(), WalkMode::Count, 100).complete);
    }

    /// Only one walk runs at a time, however many threads ask.
    #[test]
    fn walks_run_one_at_a_time() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..300 {
            std::fs::write(dir.path().join(format!("f{i}")), "x").expect("write");
        }
        let path = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        assert_eq!(count_tree(&path, &HashSet::new(), 1000).count, 300);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("walker thread");
        }
        assert_eq!(WALKS_MOST.load(Ordering::SeqCst), 1, "two walks overlapped");
    }
}
