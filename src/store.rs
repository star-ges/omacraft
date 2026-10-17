//! Content store: every mod, resource pack and shader pack Omacraft downloads is
//! kept once under `cache/content/<sha1>`, and each instance gets a reflinked copy.
//! Instances hold real, independent files (export, import and `cp -r` keep working,
//! and editing one copy never reaches another), while btrfs/xfs store the bytes once.
//! On filesystems without reflinks the store is only a staging area: the download
//! is moved into the instance, so nothing takes twice the space.
//!
//! `reclaim` merges identical files already in instances (imports, jars added by
//! hand) with the kernel's dedupe ioctl, which checks the bytes match first, and
//! drops cached files no instance uses any more.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use sha1::{Digest, Sha1};

use crate::instance::{self, ContentKind};
use crate::net::{self, Download};
use crate::paths;
use crate::task::Task;

const KINDS: [ContentKind; 3] = [ContentKind::Mods, ContentKind::ResourcePacks, ContentKind::ShaderPacks];

/// Files smaller than this aren't worth merging (and btrfs may keep them inline).
const MIN_SIZE: u64 = 16 << 10;

pub fn dir() -> PathBuf {
    paths::data_dir().join("cache").join("content")
}

fn is_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Download `d` through the store: a cached copy with the same SHA-1 is reused
/// instead of downloaded again. Without a SHA-1 this is a plain download.
pub async fn fetch(d: &Download) -> anyhow::Result<()> {
    let Some(sha1) = d.sha1.as_deref().filter(|h| is_hash(h)) else {
        return net::download(d).await;
    };
    let cached = dir().join(sha1.to_ascii_lowercase());
    let have = std::fs::metadata(&cached).is_ok_and(|m| d.size.is_none_or(|s| m.len() == s));
    if !have {
        net::download(&Download { url: d.url.clone(), path: cached.clone(), sha1: d.sha1.clone(), size: d.size }).await?;
    }
    let dest = d.path.clone();
    if tokio::task::spawn_blocking(move || place(&cached, &dest)).await?? {
        return Ok(());
    }
    // The cached file went away under us (a concurrent move or reclaim).
    net::download(d).await
}

/// Put `cached` at `dest`: a reflink, else a move (no CoW, same filesystem), else
/// a copy. Ok(false) if `cached` vanished before any of them worked.
fn place(cached: &Path, dest: &Path) -> anyhow::Result<bool> {
    let parent = dest.parent().context("no parent folder")?;
    std::fs::create_dir_all(parent)?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.{}.part", uuid::Uuid::new_v4().simple()));
    let staged = reflink_copy::reflink(cached, &tmp).is_ok() || {
        let _ = std::fs::remove_file(&tmp);
        std::fs::rename(cached, &tmp).is_ok() || std::fs::copy(cached, &tmp).is_ok()
    };
    if !staged {
        let _ = std::fs::remove_file(&tmp);
        if cached.exists() {
            bail!("couldn't copy {} into {}", cached.display(), parent.display());
        }
        return Ok(false);
    }
    std::fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(true)
}

/// What `reclaim` did.
#[derive(Debug, Default)]
pub struct Reclaimed {
    /// Instance files now sharing their bytes with an identical one.
    pub merged: usize,
    pub merged_bytes: u64,
    /// Cached downloads no instance uses any more, deleted.
    pub pruned: usize,
    pub pruned_bytes: u64,
}

impl Reclaimed {
    pub fn summary(&self) -> String {
        if self.merged == 0 && self.pruned == 0 {
            return "Nothing to reclaim: identical files already share their space".into();
        }
        let mut parts = Vec::new();
        if self.merged > 0 {
            parts.push(format!("merged {} duplicate file{} ({})", self.merged, if self.merged == 1 { "" } else { "s" }, human(self.merged_bytes)));
        }
        if self.pruned > 0 {
            parts.push(format!("cleared {} unused download{} ({})", self.pruned, if self.pruned == 1 { "" } else { "s" }, human(self.pruned_bytes)));
        }
        let s = parts.join(", ");
        s[..1].to_uppercase() + &s[1..]
    }
}

pub fn human(bytes: u64) -> String {
    match bytes {
        0..1_048_576 => format!("{:.0} KiB", bytes as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
        _ => format!("{:.2} GiB", bytes as f64 / 1_073_741_824.0),
    }
}

/// Every instance's content files (mods, resource packs, shader packs; enabled or not).
fn instance_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for inst in instance::load_all() {
        for kind in KINDS {
            out.extend(inst.content(kind).into_iter().filter(|f| !f.is_dir).map(|f| f.path));
        }
    }
    out
}

/// Merge identical content files across all instances and prune the store.
pub async fn reclaim(task: Task) -> anyhow::Result<Reclaimed> {
    tokio::task::spawn_blocking(move || {
        task.stage("Looking for duplicates", 0);
        reclaim_in(&dir(), instance_files(), &task)
    })
    .await?
}

fn reclaim_in(store: &Path, files: Vec<PathBuf>, task: &Task) -> anyhow::Result<Reclaimed> {
    std::fs::create_dir_all(store)?;
    let mut out = Reclaimed::default();
    let mut cached: HashMap<String, u64> = HashMap::new();
    for e in std::fs::read_dir(store)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match e.metadata() {
            Ok(m) if m.is_file() && is_hash(&name) => {
                cached.insert(name, m.len());
            }
            _ => {}
        }
    }
    // Only files whose size turns up twice (counting the store) can be duplicates.
    let mut sized: Vec<(PathBuf, u64)> = files
        .into_iter()
        .filter_map(|p| std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file()).map(|m| (p, m.len())))
        .collect();
    let mut count: HashMap<u64, usize> = HashMap::new();
    for len in sized.iter().map(|(_, l)| *l).chain(cached.values().copied()) {
        *count.entry(len).or_default() += 1;
    }
    sized.retain(|(_, len)| count[len] > 1);
    task.stage(format!("Hashing {} files", sized.len()), sized.iter().map(|(_, l)| l).sum());
    let mut groups: HashMap<String, Vec<(PathBuf, u64)>> = HashMap::new();
    for (path, len) in sized {
        if let Ok(h) = hash_file(&path, task) {
            groups.entry(h).or_default().push((path, len));
        }
    }
    let used: HashSet<&String> = groups.keys().collect();
    for (name, len) in &cached {
        if !used.contains(name) && std::fs::remove_file(store.join(name)).is_ok() {
            out.pruned += 1;
            out.pruned_bytes += len;
        }
    }
    let todo: Vec<_> = groups.into_iter().filter(|(_, g)| g.iter().any(|(_, l)| *l >= MIN_SIZE)).collect();
    task.stage("Merging duplicates", todo.len() as u64);
    for (hash, group) in todo {
        task.inc(1);
        // The store keeps one copy of each, so later installs can reuse it.
        let source = store.join(&hash);
        if !source.exists() && reflink_copy::reflink(&group[0].0, &source).is_err() {
            if group.len() > 1 {
                bail!("this filesystem can't share file contents between files (that needs btrfs or xfs)");
            }
            continue;
        }
        for (path, len) in &group {
            match dedupe(&source, path) {
                Ok(true) => {
                    out.merged += 1;
                    out.merged_bytes += len;
                }
                Ok(false) => {}
                Err(e) => eprintln!("merging {}: {e:#}", path.display()),
            }
        }
    }
    Ok(out)
}

/// SHA-1 of a file, read in chunks, ticking the task by bytes.
fn hash_file(path: &Path, task: &Task) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        task.inc(n as u64);
    }
    Ok(hex::encode(hasher.finalize()))
}

// -- kernel ioctls ---------------------------------------------------------------------

/// struct file_dedupe_range with one file_dedupe_range_info (linux/fs.h).
#[repr(C)]
struct DedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

/// _IOWR(0x94, 54, struct file_dedupe_range)
const FIDEDUPERANGE: libc::c_ulong = 0xC018_9436;
/// btrfs dedupes at most this much per call.
const DEDUPE_CHUNK: u64 = 16 << 20;

/// Make `dest` share `src`'s bytes on disk. The kernel compares the contents
/// first and leaves the file alone if they differ. Ok(false) if they already
/// shared every extent, or differ.
fn dedupe(src: &Path, dest: &Path) -> anyhow::Result<bool> {
    let s = std::fs::File::open(src)?;
    let d = std::fs::File::open(dest)?;
    let len = s.metadata()?.len();
    if len != d.metadata()?.len() {
        return Ok(false);
    }
    if extents(&s)? == extents(&d)? {
        return Ok(false);
    }
    let mut off = 0;
    while off < len {
        let mut r = DedupeRange {
            src_offset: off,
            src_length: DEDUPE_CHUNK.min(len - off),
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            dest_fd: d.as_raw_fd() as i64,
            dest_offset: off,
            bytes_deduped: 0,
            status: 0,
            reserved: 0,
        };
        // SAFETY: `r` is a valid file_dedupe_range with room for its one info record.
        if unsafe { libc::ioctl(s.as_raw_fd(), FIDEDUPERANGE as _, &mut r) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        match r.status {
            0 if r.bytes_deduped > 0 => off += r.bytes_deduped,
            0 => bail!("the kernel deduped nothing"),
            1 => return Ok(false),
            e => return Err(std::io::Error::from_raw_os_error(-e).into()),
        }
    }
    Ok(true)
}

/// struct fiemap with room for `FIEMAP_BATCH` extents (linux/fiemap.h).
#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [FiemapExtent; FIEMAP_BATCH],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

const FIEMAP_BATCH: usize = 64;
/// _IOWR('f', 11, struct fiemap)
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 1;
const FIEMAP_EXTENT_LAST: u32 = 1;

/// The file's extents as (logical, physical, length): two files with the same
/// list already share their bytes.
fn extents(f: &std::fs::File) -> std::io::Result<Vec<(u64, u64, u64)>> {
    let mut out = Vec::new();
    let mut start = 0;
    loop {
        let zero = FiemapExtent { logical: 0, physical: 0, length: 0, reserved64: [0; 2], flags: 0, reserved: [0; 3] };
        let mut m = Fiemap { start, length: u64::MAX - start, flags: FIEMAP_FLAG_SYNC, mapped_extents: 0, extent_count: FIEMAP_BATCH as u32, reserved: 0, extents: [zero; FIEMAP_BATCH] };
        // SAFETY: `m` is a valid struct fiemap with room for `extent_count` extents.
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, &mut m) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let got = &m.extents[..m.mapped_extents as usize];
        out.extend(got.iter().map(|e| (e.logical, e.physical, e.length)));
        match got.last() {
            Some(e) if e.flags & FIEMAP_EXTENT_LAST == 0 => start = e.logical + e.length,
            _ => return Ok(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = paths::test_root().join("store").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn reflinks_work(dir: &Path) -> bool {
        let (a, b) = (dir.join(".probe-a"), dir.join(".probe-b"));
        std::fs::write(&a, b"probe").unwrap();
        let ok = reflink_copy::reflink(&a, &b).is_ok();
        let _ = (std::fs::remove_file(&a), std::fs::remove_file(&b));
        ok
    }

    #[test]
    fn place_gives_an_independent_copy() {
        let d = scratch("place");
        let cached = d.join("cache");
        std::fs::write(&cached, b"jar bytes").unwrap();
        let dest = d.join("inst/mods/a.jar");
        assert!(place(&cached, &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"jar bytes");
        if cached.exists() {
            // Reflinked: writing one copy leaves the other alone.
            std::fs::write(&dest, b"changed").unwrap();
            assert_eq!(std::fs::read(&cached).unwrap(), b"jar bytes");
        }
        // Nothing left behind but the file itself.
        assert_eq!(std::fs::read_dir(d.join("inst/mods")).unwrap().count(), 1);
        assert!(!place(&d.join("gone"), &d.join("inst/mods/b.jar")).unwrap());
    }

    #[test]
    fn reclaim_merges_duplicates_and_prunes_the_store() {
        let d = scratch("reclaim");
        let store = d.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (a, b, c) = (d.join("a.jar"), d.join("b.jar"), d.join("c.jar"));
        std::fs::write(&a, &big).unwrap();
        std::fs::write(&b, &big).unwrap();
        let mut other = big.clone();
        other[0] ^= 1;
        std::fs::write(&c, &other).unwrap();
        // A cached download no instance uses, and a temp file that isn't a hash.
        std::fs::write(store.join("0".repeat(40)), b"stale").unwrap();
        std::fs::write(store.join(".x.part"), b"tmp").unwrap();
        let files = vec![a.clone(), b.clone(), c.clone()];
        if !reflinks_work(&d) {
            assert!(reclaim_in(&store, files, &Task::new("t")).is_err());
            return;
        }
        // a seeds the store, so only b's space is saved.
        let r = reclaim_in(&store, files.clone(), &Task::new("t")).unwrap();
        assert_eq!((r.merged, r.pruned), (1, 1), "{r:?}");
        assert!(!store.join("0".repeat(40)).exists());
        assert!(store.join(".x.part").exists());
        let fa = std::fs::File::open(&a).unwrap();
        assert_eq!(extents(&fa).unwrap(), extents(&std::fs::File::open(&b).unwrap()).unwrap());
        assert_ne!(extents(&fa).unwrap(), extents(&std::fs::File::open(&c).unwrap()).unwrap());
        assert_eq!(std::fs::read(&a).unwrap(), big);
        assert_eq!(std::fs::read(&c).unwrap(), other);
        // Merged files stay independent copies.
        std::fs::write(&a, b"changed").unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), big);
        // A second pass finds nothing new.
        std::fs::write(&a, &big).unwrap();
        let r = reclaim_in(&store, files, &Task::new("t")).unwrap();
        assert_eq!((r.merged, r.pruned), (1, 0), "{r:?}");
    }
}
