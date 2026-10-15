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
    let root = src.canonicalize()?;
    let mut skipped = Vec::new();
    copy_dir(&root, src, dst, &mut vec![root.clone()], progress, &mut skipped)?;
    Ok(skipped)
}

/// `stack` holds the canonical dirs being copied, to catch links back up the tree.
fn copy_dir(root: &Path, src: &Path, dst: &Path, stack: &mut Vec<PathBuf>, progress: &dyn Fn(u64), skipped: &mut Vec<String>) -> std::io::Result<()> {
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
            copy_dir(root, &real, &to, stack, progress, skipped)?;
            stack.pop();
        } else if ft.is_file() {
            reflink_copy::reflink_or_copy(&real, &to)?;
            progress(std::fs::metadata(&real).map(|m| m.len()).unwrap_or(0));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
