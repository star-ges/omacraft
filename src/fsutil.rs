//! Copying directory trees. Uses reflinks where the filesystem supports them
//! (btrfs, xfs), so even large instances import instantly.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Total bytes of regular files under `dir`, following links like `copy_tree`.
pub fn tree_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if !d.canonicalize().is_ok_and(|c| seen.insert(c)) {
            continue;
        }
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match std::fs::metadata(e.path()) {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) if m.is_file() => total += m.len(),
                _ => {}
            }
        }
    }
    total
}

/// Replace `path` with `bytes` without ever leaving it half-written: write a
/// uniquely named file beside it, sync, then rename over. `mode` sets the
/// permissions of the result (the old file's mode is not kept).
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// An exclusive `flock` on a lock file, held until dropped. It excludes other
/// processes as well as other threads here (each lock opens the file anew).
pub struct FileLock(#[allow(dead_code)] std::fs::File);

fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)
}

/// Whether the locked `f` is still the file at `path`: whoever held it may have deleted
/// it (only ever with the lock held), and a lock on the unlinked file excludes no one.
fn still_there(f: &std::fs::File, path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = f.metadata()?;
    Ok(std::fs::metadata(path).is_ok_and(|m| m.ino() == held.ino() && m.dev() == held.dev()))
}

/// Take the lock at `path`, waiting for whoever holds it. Delete a lock file only while
/// holding it: everyone else then finds it gone and locks the new one.
pub fn lock(path: &Path) -> std::io::Result<FileLock> {
    loop {
        let f = lock_file(path)?;
        f.lock()?;
        if still_there(&f, path)? {
            return Ok(FileLock(f));
        }
    }
}

/// Take the lock at `path` if nobody holds it.
pub fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
    loop {
        let f = lock_file(path)?;
        match f.try_lock() {
            Ok(()) if still_there(&f, path)? => return Ok(Some(FileLock(f))),
            Ok(()) => continue,
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

fn renameat2(a: &Path, b: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (a, b) = (c(a)?, c(b)?);
    if unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The filesystem can't do this kind of rename.
fn unsupported(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP))
}

/// Swap two paths in one step (`renameat2` with `RENAME_EXCHANGE`).
pub fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::NO_EXCHANGE.with(|n| n.get()) {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    renameat2(a, b, libc::RENAME_EXCHANGE)
}

/// Rename `from` to `to` unless something is already at `to` (then `AlreadyExists`,
/// and nothing moves). Falls back to check-then-rename where the filesystem lacks
/// `RENAME_NOREPLACE`.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    match renameat2(from, to, libc::RENAME_NOREPLACE) {
        Err(e) if unsupported(&e) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            std::fs::rename(from, to)
        }
        r => r,
    }
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// What [`swap_in`] is doing, saved beside the staged folder before anything moves,
/// so [`finish_swap`] can tell after a crash whether the copy is in place yet: it
/// knows the copy by its inode, which a rename keeps.
#[derive(serde::Serialize, serde::Deserialize)]
struct Swap {
    target: PathBuf,
    dev: u64,
    ino: u64,
}

/// `staged` + `suffix`, beside it.
fn beside(staged: &Path, suffix: &str) -> PathBuf {
    let mut s = staged.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The journal [`swap_in`] keeps for the folder staged at `staged`.
pub fn swap_journal(staged: &Path) -> PathBuf {
    beside(staged, ".swap")
}

fn identity(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

/// Put the whole folder `staged` where `target` is, replacing whatever is there.
/// Swapped in one step where the filesystem can; elsewhere the old one is renamed
/// aside first. A journal (`<staged>.swap`) records the move, so a crash at any step
/// is finished by [`finish_swap`] and never leaves `target` missing for good.
pub fn swap_in(staged: &Path, target: &Path) -> std::io::Result<()> {
    let (dev, ino) = identity(staged).ok_or(std::io::ErrorKind::NotFound)?;
    let swap = Swap { target: target.to_path_buf(), dev, ino };
    let journal = swap_journal(staged);
    write_atomic(&journal, toml::to_string(&swap).map_err(std::io::Error::other)?.as_bytes(), 0o644)?;
    finish(staged, &journal, &swap)
}

/// Finish (or clean up after) a [`swap_in`] of `staged` cut short. Returns false if
/// there was no journal: a folder at `staged` is then a copy that never finished.
pub fn finish_swap(staged: &Path) -> std::io::Result<bool> {
    let journal = swap_journal(staged);
    let Ok(text) = std::fs::read_to_string(&journal) else { return Ok(false) };
    let swap: Swap = toml::from_str(&text).map_err(std::io::Error::other)?;
    finish(staged, &journal, &swap)?;
    Ok(true)
}

fn finish(staged: &Path, journal: &Path, swap: &Swap) -> std::io::Result<()> {
    let ours = Some((swap.dev, swap.ino));
    let old = beside(staged, ".old");
    let target = &swap.target;
    if identity(target) != ours {
        if identity(staged) != ours {
            // Neither holds the copy: it's gone. Put back what was renamed aside, if anything.
            if std::fs::symlink_metadata(target).is_err() && std::fs::symlink_metadata(&old).is_ok() {
                std::fs::rename(&old, target)?;
            }
            return std::fs::remove_file(journal);
        }
        if std::fs::symlink_metadata(target).is_err() {
            std::fs::rename(staged, target)?;
        } else {
            match exchange(staged, target) {
                Ok(()) => {}
                Err(e) if unsupported(&e) => {
                    // Aside first, never deleted before the copy is in place.
                    remove_any(&old)?;
                    std::fs::rename(target, &old)?;
                    std::fs::rename(staged, target)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    // In place. What's left at `staged` (after an exchange) or `.old` is the old folder.
    if identity(staged) != ours {
        remove_any(staged)?;
    }
    remove_any(&old)?;
    std::fs::remove_file(journal)
}

/// The most we decompress for one metadata entry (instance.cfg, modrinth.index.json,
/// mod metadata and icons, level.dat), so a crafted archive can't exhaust memory.
pub const METADATA_LIMIT: u64 = 16 << 20;

/// Read all of `r`, failing cleanly once it passes `METADATA_LIMIT` bytes.
pub fn read_capped(r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    r.take(METADATA_LIMIT + 1).read_to_end(&mut out)?;
    if out.len() as u64 > METADATA_LIMIT {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("larger than {} MiB", METADATA_LIMIT >> 20)));
    }
    Ok(out)
}

/// Recursively copy `src` into `dst`, calling `progress` with bytes copied.
///
/// The copy is self-contained: every symlink is replaced by a copy of what it
/// points to, inside the instance or not (Prism users link shared `saves` or
/// `mods` folders), so later edits never reach the original. Broken links and
/// links back up the tree are skipped. Returns a note for each skipped link.
pub fn copy_tree(src: &Path, dst: &Path, progress: &dyn Fn(u64)) -> std::io::Result<Vec<String>> {
    copy_tree_outside(src, dst, None, progress)
}

/// [`copy_tree`], failing if any folder it copies, reached through links or not, holds
/// `keep_out` or lies inside it (canonical paths). With `keep_out` the folder the copy
/// is made in, a source that holds it would copy its own copy over and over.
pub fn copy_tree_outside(src: &Path, dst: &Path, keep_out: Option<&Path>, progress: &dyn Fn(u64)) -> std::io::Result<Vec<String>> {
    let root = src.canonicalize()?;
    let mut skipped = Vec::new();
    copy_dir(&root, src, dst, keep_out, &mut vec![root.clone()], progress, &mut skipped)?;
    Ok(skipped)
}

/// Whether copying the canonical folder `dir` would reach `keep_out`.
fn reaches(dir: &Path, keep_out: Option<&Path>) -> bool {
    keep_out.is_some_and(|k| k.starts_with(dir) || dir.starts_with(k))
}

/// `stack` holds the canonical dirs being copied, to catch links back up the tree.
fn copy_dir(root: &Path, src: &Path, dst: &Path, keep_out: Option<&Path>, stack: &mut Vec<PathBuf>, progress: &dyn Fn(u64), skipped: &mut Vec<String>) -> std::io::Result<()> {
    if let Some(dir) = stack.last().filter(|d| reaches(d, keep_out)) {
        return Err(std::io::Error::other(format!("{} holds the folder it would be copied into", dir.display())));
    }
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (from, to) = (e.path(), dst.join(e.file_name()));
        let mut ft = e.file_type()?;
        let mut real = from.clone();
        if ft.is_symlink() {
            match from.canonicalize() {
                // A folder holding the whole source would copy it into itself.
                Ok(t) if root.starts_with(&t) => {
                    skipped.push(format!("skipped {} (links to a folder containing the instance)", to.display()));
                    continue;
                }
                Ok(t) => {
                    real = t;
                    ft = std::fs::metadata(&real)?.file_type();
                }
                Err(_) => {
                    skipped.push(format!("skipped {} (broken link)", to.display()));
                    continue;
                }
            }
        }
        if ft.is_dir() {
            let canon = real.canonicalize()?;
            if stack.contains(&canon) {
                skipped.push(format!("skipped {} (link loops back to a parent folder)", to.display()));
                continue;
            }
            stack.push(canon);
            copy_dir(root, &real, &to, keep_out, stack, progress, skipped)?;
            stack.pop();
        } else if ft.is_file() {
            reflink_copy::reflink_or_copy(&real, &to)?;
            progress(std::fs::metadata(&real).map(|m| m.len()).unwrap_or(0));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn scratch() -> PathBuf {
        let d = crate::paths::test_root().join(format!("fsutil-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn read_capped_stops_at_limit() {
        let n = METADATA_LIMIT as usize;
        assert_eq!(read_capped(&vec![7u8; n][..]).unwrap().len(), n);
        let err = read_capped(std::io::repeat(0)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn copy_tree_copies_through_links() {
        let d = scratch();
        let (src, outside, dst) = (d.join("src"), d.join("outside"), d.join("dst"));
        std::fs::create_dir_all(src.join("real/saves/World")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(src.join("real/saves/World/level.dat"), "w").unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        symlink(src.join("real/saves"), src.join("saves")).unwrap();
        symlink("real/saves/World/level.dat", src.join("rel-link")).unwrap();
        symlink(&outside, src.join("escape")).unwrap();
        symlink("../outside/secret", src.join("escape-file")).unwrap();
        symlink(src.join("nope"), src.join("dangling")).unwrap();
        symlink(&src, src.join("real/loop")).unwrap();
        symlink(&d, src.join("up")).unwrap();

        let skipped = copy_tree(&src, &dst, &|_| {}).unwrap();
        // Inner links become independent copies.
        assert!(!std::fs::symlink_metadata(dst.join("saves")).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(dst.join("saves/World/level.dat")).unwrap(), "w");
        assert_eq!(std::fs::read_to_string(dst.join("rel-link")).unwrap(), "w");
        std::fs::remove_dir_all(dst.join("saves/World")).unwrap();
        assert!(src.join("real/saves/World/level.dat").exists(), "deleting from the copy reached the source");
        // Links outside are copied too, as real files.
        assert!(!std::fs::symlink_metadata(dst.join("escape")).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(dst.join("escape/secret")).unwrap(), "s");
        assert_eq!(std::fs::read_to_string(dst.join("escape-file")).unwrap(), "s");
        std::fs::remove_file(dst.join("escape/secret")).unwrap();
        assert!(outside.join("secret").exists());
        // Broken and looping links are left out, with a note each.
        for p in ["dangling", "real/loop", "up"] {
            assert!(std::fs::symlink_metadata(dst.join(p)).is_err(), "{p} was copied");
        }
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        std::fs::remove_dir_all(d).unwrap();
    }

    thread_local! {
        /// Makes [`exchange`] fail as on filesystems without `RENAME_EXCHANGE`.
        pub static NO_EXCHANGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// The journal [`swap_in`] writes before moving anything, for tests that stop
    /// a swap partway.
    pub fn write_journal(staged: &Path, target: &Path) {
        let (dev, ino) = identity(staged).unwrap();
        write_atomic(&swap_journal(staged), toml::to_string(&Swap { target: target.to_path_buf(), dev, ino }).unwrap().as_bytes(), 0o644).unwrap();
    }

    fn folder(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("f"), text).unwrap();
    }

    #[test]
    fn swap_in_finishes_after_a_crash_at_any_step() {
        for no_exchange in [false, true] {
            NO_EXCHANGE.with(|n| n.set(no_exchange));
            let d = scratch();
            let (staged, target) = (d.join(".stage"), d.join("target"));
            let old = beside(&staged, ".old");
            let reset = || {
                for p in [&staged, &target, &old] {
                    let _ = remove_any(p);
                }
                folder(&target, "old");
                folder(&staged, "new");
            };
            let check = |what: &str| {
                assert_eq!(std::fs::read_to_string(target.join("f")).unwrap(), "new", "{what}");
                for p in [&staged, &old, &swap_journal(&staged)] {
                    assert!(std::fs::symlink_metadata(p).is_err(), "{what}: {} left", p.display());
                }
            };
            // A straight run.
            reset();
            swap_in(&staged, &target).unwrap();
            check("whole");
            // Each state a crash can leave, journal written:
            let journal = |staged: &Path| write_journal(staged, &target);
            // nothing moved yet;
            reset();
            journal(&staged);
            assert!(finish_swap(&staged).unwrap());
            check("before moving");
            // the old one renamed aside, the copy not yet in (no exchange): the target is missing;
            reset();
            journal(&staged);
            std::fs::rename(&target, &old).unwrap();
            assert!(finish_swap(&staged).unwrap());
            check("target aside");
            // the copy in, the old one still aside, or (after an exchange) at the stage's name.
            reset();
            journal(&staged);
            std::fs::rename(&target, &old).unwrap();
            std::fs::rename(&staged, &target).unwrap();
            assert!(finish_swap(&staged).unwrap());
            check("old aside");
            reset();
            journal(&staged);
            renameat2(&staged, &target, libc::RENAME_EXCHANGE).unwrap();
            assert_eq!(std::fs::read_to_string(staged.join("f")).unwrap(), "old");
            assert!(finish_swap(&staged).unwrap());
            check("exchanged");
            // No journal: nothing to finish.
            reset();
            assert!(!finish_swap(&staged).unwrap());
            assert_eq!(std::fs::read_to_string(target.join("f")).unwrap(), "old");
            std::fs::remove_dir_all(d).unwrap();
        }
        NO_EXCHANGE.with(|n| n.set(false));
    }

    #[test]
    fn rename_noreplace_never_replaces() {
        let d = scratch();
        std::fs::write(d.join("a"), "a").unwrap();
        std::fs::write(d.join("b"), "b").unwrap();
        assert_eq!(rename_noreplace(&d.join("a"), &d.join("b")).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(d.join("b")).unwrap(), "b");
        rename_noreplace(&d.join("a"), &d.join("c")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("c")).unwrap(), "a");
        std::fs::remove_dir_all(d).unwrap();
    }

    /// A child process (this test binary again, running [`hold_flock_helper`]) holding
    /// the lock at a path until dropped.
    pub struct FlockHolder(std::process::Child);

    impl FlockHolder {
        pub fn lock(path: &Path) -> Self {
            use std::io::BufRead;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fsutil::tests::hold_flock_helper", "--ignored", "--nocapture", "--test-threads=1"])
                .env("OMACRAFT_TEST_FLOCK", path)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let out = std::io::BufReader::new(child.stdout.take().unwrap());
            assert!(out.lines().map_while(Result::ok).any(|l| l.ends_with("omacraft-test-flocked")), "helper couldn't lock");
            FlockHolder(child)
        }
    }

    impl Drop for FlockHolder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "helper, run by FlockHolder in a child process"]
    fn hold_flock_helper() {
        use std::io::Write;
        let Some(path) = std::env::var_os("OMACRAFT_TEST_FLOCK") else { return };
        let _l = lock(Path::new(&path)).unwrap();
        println!("omacraft-test-flocked");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn locks_exclude_other_processes() {
        let d = scratch();
        let path = d.join(".lock");
        let holder = FlockHolder::lock(&path);
        assert!(try_lock(&path).unwrap().is_none());
        drop(holder);
        let held = try_lock(&path).unwrap();
        assert!(held.is_some());
        // And other threads here.
        let p = path.clone();
        assert!(std::thread::spawn(move || try_lock(&p).unwrap().is_none()).join().unwrap());
        drop(held);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_lock_file_deleted_by_its_holder_still_excludes() {
        let d = scratch();
        let path = d.join(".lock");
        let held = lock(&path).unwrap();
        // Someone opened the old file and waits on it while the holder deletes it.
        let p = path.clone();
        let waiter = std::thread::spawn(move || lock(&p).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::remove_file(&path).unwrap();
        let newcomer = try_lock(&path).unwrap().expect("nobody holds the new file yet");
        drop(held);
        // The waiter got the old, unlinked file: it must notice and queue on the new one.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished(), "two holders at once");
        drop(newcomer);
        drop(waiter.join().unwrap());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_sets_mode() {
        let d = scratch();
        let p = d.join("accounts.json");
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&p, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "temp file left behind");
        std::fs::remove_dir_all(d).unwrap();
    }
}
