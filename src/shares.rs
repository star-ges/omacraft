//! Shared folders. Minecraft has no setting for where packs live, so sharing turns
//! the instance's folder into a link to one under `shared/`:
//! `shared/sets/<name>/{resourcepacks,shaderpacks,schematics}`, one pool per share
//! set, a named set of instances (packs are tied to versions, so one global pool
//! fills up with packs half the instances can't use). World sets do the same for
//! `saves`, under `shared/worlds/<name>/saves`; worlds.rs keeps their members on one
//! version and loader and checks each world's mods before launch.
//!
//! `apply` makes an instance's folders match its settings. Sharing moves the
//! instance's files into the shared folder (a name clash keeps both and renames the
//! incoming one; identical files are kept once). Unsharing leaves the instance a full
//! copy (reflinks, so free on btrfs). Links are relative, so the data dir can move.
//!
//! Screenshots aren't shared on disk: the screenshots tab can show every instance's
//! at once instead.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, bail};

use crate::instance::Instance;
use crate::{fsutil, paths};

/// Folders a share set shares.
pub const SET_FOLDERS: [&str; 3] = ["resourcepacks", "shaderpacks", "schematics"];

/// Folders a world set shares.
pub const WORLD_FOLDERS: [&str; 1] = ["saves"];

/// What a set shares: packs (`share_set`) or worlds (`world_set`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Packs,
    Worlds,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Packs, Kind::Worlds];

    pub fn folders(self) -> &'static [&'static str] {
        match self {
            Kind::Packs => &SET_FOLDERS,
            Kind::Worlds => &WORLD_FOLDERS,
        }
    }

    /// The folder under `shared/` holding this kind's sets.
    fn root(self) -> &'static str {
        match self {
            Kind::Packs => "sets",
            Kind::Worlds => "worlds",
        }
    }

    fn of(folder: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.folders().contains(&folder))
    }

    /// The set of this kind the instance is in ("" for none).
    pub fn set_of(self, inst: &Instance) -> &str {
        match self {
            Kind::Packs => &inst.share_set,
            Kind::Worlds => &inst.world_set,
        }
    }

    /// For messages: "share set" or "world set".
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Packs => "share set",
            Kind::Worlds => "world set",
        }
    }
}

/// From an instance's game dir (`instances/<id>/minecraft`) back to the data dir.
const UP: &str = "../../..";

/// One `apply` at a time, so two never move the same files.
static LOCK: Mutex<()> = Mutex::new(());

pub fn dir() -> PathBuf {
    paths::data_dir().join("shared")
}

pub fn sets_dir(kind: Kind) -> PathBuf {
    dir().join(kind.root())
}

pub fn set_dir(kind: Kind, name: &str) -> PathBuf {
    sets_dir(kind).join(name)
}

/// A set's name doubles as its folder name.
pub fn check_set_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) || name.chars().any(char::is_control) || name.len() > 64 {
        bail!("“{name}” can't be a set name: use up to 64 characters, no slashes, not starting with a dot");
    }
    Ok(())
}

/// Sets of this kind with a folder on disk, sorted. Reads one directory.
pub fn sets_on_disk(kind: Kind) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(sets_dir(kind))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| check_set_name(n).is_ok())
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

/// Delete a set's folder and everything in it. Refuses while any instance's folder
/// still links into it (say, one whose leave failed halfway), whatever its settings
/// say: deleting would take that instance's only packs or worlds with it.
pub fn delete_set(kind: Kind, name: &str) -> anyhow::Result<()> {
    check_set_name(name)?;
    if let Some(ids) = sets_in_use(kind).remove(name) {
        bail!("“{name}” is still linked from {} (Omacraft finishes leaving it at the next launch)", ids.join(", "));
    }
    std::fs::remove_dir_all(set_dir(kind, name)).with_context(|| format!("deleting {} {name}", kind.noun()))
}

/// Set name → ids of the instances whose folders link into it now. This is what's
/// on disk, which lags `share_set`/`world_set` until a change finishes. Reads a few
/// links per instance.
pub fn sets_in_use(kind: Kind) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in std::fs::read_dir(paths::instances_dir()).into_iter().flatten().flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        let game = e.path().join("minecraft");
        let mut sets: Vec<String> = kind.folders().iter().filter_map(|f| linked_set(&game.join(f))).collect();
        sets.dedup();
        for set in sets {
            out.entry(set).or_default().push(id.clone());
        }
    }
    out
}

/// The share set `link` points into, if it's a link to one.
fn linked_set(link: &Path) -> Option<String> {
    link_set(link.file_name()?.to_str()?, &std::fs::read_link(link).ok()?)
}

/// Where `folder` in the game dir should link to, relative to the game dir.
fn target(inst: &Instance, folder: &str) -> Option<PathBuf> {
    let kind = Kind::of(folder)?;
    let set = kind.set_of(inst);
    (!set.is_empty()).then(|| PathBuf::from(format!("{UP}/shared/{}/{set}/{folder}", kind.root())))
}

/// The set a link from the game dir's `folder` names, if its target has exactly the
/// shape Omacraft makes: `../../../shared/<sets|worlds>/<set>/<folder>`, or (made
/// elsewhere) the absolute path to it. Nothing else, so no `..` or other folder.
/// Only looks at the text; [`lands_in_set`] checks where it really leads.
fn link_set(folder: &str, target: &Path) -> Option<String> {
    let kind = Kind::of(folder)?;
    let rest = target.strip_prefix(format!("{UP}/shared/{}", kind.root())).or_else(|_| target.strip_prefix(sets_dir(kind))).ok()?;
    let mut parts = rest.components();
    let (Some(Component::Normal(set)), Some(Component::Normal(f)), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let set = set.to_str()?;
    (f == folder && check_set_name(set).is_ok()).then(|| set.to_string())
}

/// Whether `path` really leads to `shared/<sets|worlds>/<set>/<folder>`, following
/// every link on the way (a set folder swapped for a link elsewhere doesn't count).
fn lands_in_set(path: &Path, set: &str, folder: &str) -> bool {
    let Some(kind) = Kind::of(folder) else { return false };
    match (path.canonicalize(), sets_dir(kind).canonicalize()) {
        (Ok(real), Ok(sets)) => real == sets.join(set).join(folder),
        _ => false,
    }
}

/// Whether `path` is a link to a set's folder that really leads there (export
/// follows these, so nothing else may pass).
pub fn is_share_link(path: &Path) -> bool {
    let Some(folder) = path.file_name().and_then(|f| f.to_str()) else { return false };
    std::fs::read_link(path).ok().and_then(|t| link_set(folder, &t)).is_some_and(|set| lands_in_set(path, &set, folder))
}

/// Make the instance's folders match its settings: linked to the shared folders
/// it uses, real folders for the rest. Returns notes (renamed clashes, skipped
/// links) for the user. Safe to run again after an interruption.
pub fn apply(inst: &Instance) -> anyhow::Result<Vec<String>> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for kind in Kind::ALL {
        let set = kind.set_of(inst);
        if !set.is_empty() {
            check_set_name(set)?;
        }
    }
    let game = inst.game_dir();
    std::fs::create_dir_all(&game)?;
    let mut notes = Vec::new();
    for &folder in Kind::ALL.iter().flat_map(|k| k.folders()) {
        let set = Kind::of(folder).map_or("", |k| k.set_of(inst));
        let link = game.join(folder);
        let want = target(inst, folder);
        recover(&link, folder, want.is_some(), &mut notes).with_context(|| format!("finishing an interrupted change to {folder}"))?;
        let current = std::fs::read_link(&link).ok();
        match (want, current) {
            (Some(w), Some(c)) if w == c => std::fs::create_dir_all(game.join(&w))?,
            // Moved to another share set: the files stay with the old one.
            (Some(w), Some(c)) if link_set(folder, &c).is_some() => {
                std::fs::create_dir_all(game.join(&w))?;
                link_to(&link, &w)?;
            }
            (None, Some(c)) if let Some(set) = link_set(folder, &c) => unshare(&link, &set, &mut notes).with_context(|| format!("unsharing {folder}"))?,
            (Some(_), Some(_)) => notes.push(format!("{}: left {folder} alone, it's a link Omacraft didn't make", inst.name)),
            (Some(w), None) => {
                let shared = game.join(&w);
                std::fs::create_dir_all(&shared)?;
                if !lands_in_set(&shared, set, folder) {
                    bail!("{} leads outside the shared folders; left {folder} alone", shared.display());
                }
                match std::fs::symlink_metadata(&link) {
                    Ok(m) if m.is_dir() => {
                        // Pack folders (unzipped packs) and worlds clash as a whole; schematics merge.
                        let merge = folder == "schematics";
                        copy_through_links(&link, &mut notes).with_context(|| format!("can't share {folder}"))?;
                        move_into(&link, &shared, merge, &inst.id, &mut notes).with_context(|| format!("moving {folder} into the shared folder"))?;
                    }
                    Ok(_) => {
                        notes.push(format!("{}: {folder} is a file, not a folder; left it alone", inst.name));
                        continue;
                    }
                    Err(_) => {}
                }
                link_to(&link, &w)?;
            }
            (None, _) => {}
        }
    }
    Ok(notes)
}

/// Point `link` at `target`, replacing whatever link is there in one step.
fn link_to(link: &Path, target: &Path) -> std::io::Result<()> {
    let tmp = link.with_file_name(format!(".{}.{}.link", link.file_name().unwrap_or_default().to_string_lossy(), uuid::Uuid::new_v4().simple()));
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Where leaving a set stages the instance's copy of `link`: `.<folder>.copying`
/// while it's made, `.<folder>.copied` once it's whole.
fn staging(link: &Path, stage: &str) -> PathBuf {
    link.with_file_name(format!(".{}.{stage}", link.file_name().unwrap_or_default().to_string_lossy()))
}

/// Replace a link to share set `set` with a full copy of what it points to. The copy
/// is made under a fixed staging name and swapped in once whole, so [`recover`] can
/// finish or discard whatever an interruption leaves.
fn unshare(link: &Path, set: &str, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let name = link.file_name().unwrap_or_default().to_string_lossy();
    let (copying, copied) = (staging(link, "copying"), staging(link, "copied"));
    let made = match link.canonicalize() {
        Ok(_) if !lands_in_set(link, set, &name) => bail!("it leads outside the shared folders; left it alone"),
        Ok(src) => fsutil::copy_tree(&src, &copying, &|_| {}),
        // The shared folder is gone; start empty.
        Err(_) => std::fs::create_dir(&copying).map(|_| Vec::new()),
    };
    match made {
        Ok(skipped) => notes.extend(skipped),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&copying);
            return Err(e.into());
        }
    }
    std::fs::rename(&copying, &copied)?;
    publish(link, &copied)
}

/// Put the finished copy where `link` is: swapped in one step where the filesystem
/// can, then the old link (now under the staging name) removed.
fn publish(link: &Path, copied: &Path) -> anyhow::Result<()> {
    match exchange(copied, link) {
        Ok(()) => std::fs::remove_file(copied)?,
        // No atomic swap here; `recover` covers the moment with neither in place.
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)) => {
            std::fs::remove_file(link)?;
            std::fs::rename(copied, link)?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Swap two paths in one step (`renameat2` with `RENAME_EXCHANGE`).
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (a, b) = (c(a)?, c(b)?);
    if unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_EXCHANGE) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Clean up after a leave that was cut short at `link`: drop a half-made copy, drop
/// the old link if it was already swapped out, and put a whole copy in place if the
/// instance is still leaving (or discard it if it has joined a set again since).
fn recover(link: &Path, folder: &str, want_shared: bool, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let (copying, copied) = (staging(link, "copying"), staging(link, "copied"));
    if std::fs::symlink_metadata(&copying).is_ok_and(|m| m.is_dir()) {
        std::fs::remove_dir_all(&copying)?;
    }
    let Ok(m) = std::fs::symlink_metadata(&copied) else { return Ok(()) };
    if m.is_symlink() {
        return Ok(std::fs::remove_file(&copied)?);
    }
    if !m.is_dir() {
        return Ok(());
    }
    let current = std::fs::symlink_metadata(link).ok();
    let ours = std::fs::read_link(link).ok().is_some_and(|t| link_set(folder, &t).is_some());
    match (want_shared, current) {
        (true, _) => std::fs::remove_dir_all(&copied)?,
        (false, None) => std::fs::rename(&copied, link)?,
        (false, Some(_)) if ours => publish(link, &copied)?,
        (false, Some(_)) => notes.push(format!("{} holds an unfinished copy of {folder}; left it for you to check", copied.display())),
    }
    Ok(())
}

/// Move everything in `src` into `dst`, then remove `src`. On a clash, identical
/// files are kept once, folders merge if `merge` is set, and anything else comes
/// in renamed with `tag`.
fn move_into(src: &Path, dst: &Path, merge: bool, tag: &str, notes: &mut Vec<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let (from, to) = (e.path(), dst.join(&name));
        let ft = e.file_type()?;
        match std::fs::symlink_metadata(&to) {
            Err(_) => move_entry(&from, &to)?,
            Ok(m) if merge && ft.is_dir() && m.is_dir() => move_into(&from, &to, merge, tag, notes)?,
            Ok(m) if ft.is_file() && m.is_file() && same_bytes(&from, &to)? => std::fs::remove_file(&from)?,
            Ok(_) => {
                let to = free_name(dst, &name, tag);
                notes.push(format!("{name} was in both; this instance's copy is now {}", to.file_name().unwrap_or_default().to_string_lossy()));
                move_entry(&from, &to)?;
            }
        }
    }
    std::fs::remove_dir(src)?;
    Ok(())
}

/// Replace every link under `dir` with a copy of what it points to, so the files read
/// the same once moved (a relative link would point somewhere else from the shared
/// folder), as folder imports do. Changes nothing, and fails naming them, if any link
/// is broken or loops back up the tree.
fn copy_through_links(dir: &Path, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let mut links = Vec::new();
    find_links(dir, &mut links)?;
    let bad: Vec<String> = links
        .iter()
        .filter(|l| match (l.canonicalize(), l.parent().map(Path::canonicalize)) {
            (Ok(t), Some(Ok(parent))) => parent.starts_with(&t),
            _ => true,
        })
        .map(|l| l.strip_prefix(dir).unwrap_or(l).display().to_string())
        .collect();
    if !bad.is_empty() {
        bail!("these links are broken or loop back up the folder: {}; fix or remove them first", bad.join(", "));
    }
    for link in links {
        let tmp = link.with_file_name(format!(".{}.{}.copy", link.file_name().unwrap_or_default().to_string_lossy(), uuid::Uuid::new_v4().simple()));
        let real = link.canonicalize()?;
        let copied = if real.is_dir() { fsutil::copy_tree(&real, &tmp, &|_| {}) } else { reflink_copy::reflink_or_copy(&real, &tmp).map(|_| Vec::new()) };
        match copied {
            Ok(skipped) => notes.extend(skipped),
            Err(e) => {
                let _ = if tmp.is_dir() { std::fs::remove_dir_all(&tmp) } else { std::fs::remove_file(&tmp) };
                return Err(e.into());
            }
        }
        match exchange(&tmp, &link) {
            Ok(()) => std::fs::remove_file(&tmp)?,
            Err(_) => {
                std::fs::remove_file(&link)?;
                std::fs::rename(&tmp, &link)?;
            }
        }
        let rel = link.strip_prefix(dir.parent().unwrap_or(dir)).unwrap_or(&link);
        notes.push(format!("{} was a link; it's now a copy of {}", rel.display(), real.display()));
    }
    Ok(())
}

/// Every symlink under `dir`, not following any.
fn find_links(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        if ft.is_symlink() {
            out.push(e.path());
        } else if ft.is_dir() {
            find_links(&e.path(), out)?;
        }
    }
    Ok(())
}

/// Rename, or copy and delete across filesystems.
fn move_entry(from: &Path, to: &Path) -> anyhow::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            if std::fs::symlink_metadata(from)?.is_dir() {
                // Delete the source only once every file in it has arrived.
                let skipped = fsutil::copy_tree(from, to, &|_| {})?;
                if !skipped.is_empty() {
                    bail!("{} couldn't be moved whole: {}", from.display(), skipped.join("; "));
                }
                std::fs::remove_dir_all(from)?;
            } else {
                reflink_copy::reflink_or_copy(from, to)?;
                std::fs::remove_file(from)?;
            }
            Ok(())
        }
        r => Ok(r?),
    }
}

/// `pack.zip` → `pack (tag).zip`, then `pack (tag 2).zip`…, keeping `.disabled` last.
fn free_name(dir: &Path, name: &str, tag: &str) -> PathBuf {
    let (base, disabled) = match name.strip_suffix(".disabled") {
        Some(b) => (b, ".disabled"),
        None => (name, ""),
    };
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => base.split_at(i),
        _ => (base, ""),
    };
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{stem} ({tag}){ext}{disabled}") } else { format!("{stem} ({tag} {n}){ext}{disabled}") }))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap()
}

fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;

    fn instance(name: &str, set: &str, files: &[(&str, &str)]) -> Instance {
        let mut i = Instance::new(name, "1.21.1", Loader::Vanilla);
        i.share_set = set.into();
        i.assign_id().unwrap();
        i.save().unwrap();
        for (rel, text) in files {
            let p = i.game_dir().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        i
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn set_name() -> String {
        format!("set {}", uuid::Uuid::new_v4().simple())
    }

    #[test]
    fn sharing_moves_files_in_and_keeps_both_on_a_clash() {
        let set = set_name();
        let a = instance("a", &set, &[("resourcepacks/a.zip", "a"), ("resourcepacks/same.zip", "x"), ("resourcepacks/clash.zip", "A"), ("schematics/house/a.litematic", "a")]);
        let b = instance("b", &set, &[("resourcepacks/b.zip", "b"), ("resourcepacks/same.zip", "x"), ("resourcepacks/clash.zip", "B"), ("schematics/house/b.litematic", "b")]);
        assert!(apply(&a).unwrap().is_empty());
        let notes = apply(&b).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");

        let shared = set_dir(Kind::Packs, &set).join("resourcepacks");
        let clash = format!("clash ({}).zip", b.id);
        let mut want = vec!["a.zip".to_string(), "b.zip".into(), "clash.zip".into(), clash.clone(), "same.zip".into()];
        want.sort();
        assert_eq!(names(&shared), want);
        assert_eq!(std::fs::read_to_string(shared.join("clash.zip")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(shared.join(&clash)).unwrap(), "B");
        // Both instances see the same folder; folders inside schematics merge.
        for i in [&a, &b] {
            assert!(std::fs::symlink_metadata(i.game_dir().join("resourcepacks")).unwrap().is_symlink());
            assert_eq!(names(&i.game_dir().join("resourcepacks")), want);
            assert_eq!(names(&i.game_dir().join("schematics/house")), ["a.litematic", "b.litematic"]);
            assert!(i.game_dir().join("shaderpacks").is_dir());
        }
        // Running it again changes nothing.
        assert!(apply(&b).unwrap().is_empty());
        assert_eq!(names(&shared), want);

        // Deleting an instance removes its link, never the shared files.
        std::fs::remove_dir_all(b.dir()).unwrap();
        assert_eq!(names(&shared), want);
        std::fs::remove_dir_all(a.dir()).unwrap();
        delete_set(Kind::Packs, &set).unwrap();
    }

    #[test]
    fn unsharing_leaves_a_full_copy() {
        let set = set_name();
        let mut a = instance("a", &set, &[("shaderpacks/s.zip", "s")]);
        let b = instance("b", &set, &[]);
        apply(&a).unwrap();
        apply(&b).unwrap();
        a.share_set.clear();
        apply(&a).unwrap();
        let own = a.game_dir().join("shaderpacks");
        assert!(!std::fs::symlink_metadata(&own).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(own.join("s.zip")).unwrap(), "s");
        // The copy is independent: removing it leaves the set's file alone.
        std::fs::remove_file(own.join("s.zip")).unwrap();
        assert_eq!(names(&b.game_dir().join("shaderpacks")), ["s.zip"]);
        for i in [&a, &b] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        delete_set(Kind::Packs, &set).unwrap();
    }

    #[test]
    fn a_failed_leave_keeps_the_set_from_being_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let set = set_name();
        let mut a = instance("a", &set, &[("resourcepacks/p.zip", "p")]);
        apply(&a).unwrap();
        // The copy out of the set fails partway, as on a full disk.
        let pack = set_dir(Kind::Packs, &set).join("resourcepacks/p.zip");
        std::fs::set_permissions(&pack, std::fs::Permissions::from_mode(0o000)).unwrap();
        a.share_set.clear();
        a.save().unwrap();
        assert!(apply(&a).is_err());
        assert_eq!(sets_in_use(Kind::Packs).get(&set), Some(&vec![a.id.clone()]));
        // No instance names the set any more, but deleting it would take a's packs.
        assert!(delete_set(Kind::Packs, &set).is_err());
        assert!(std::fs::symlink_metadata(&pack).is_ok());

        // The next try finishes leaving; then the set can go.
        std::fs::set_permissions(&pack, std::fs::Permissions::from_mode(0o644)).unwrap();
        apply(&a).unwrap();
        assert_eq!(std::fs::read_to_string(a.game_dir().join("resourcepacks/p.zip")).unwrap(), "p");
        assert!(!sets_in_use(Kind::Packs).contains_key(&set));
        delete_set(Kind::Packs, &set).unwrap();
        std::fs::remove_dir_all(a.dir()).unwrap();
    }

    #[test]
    fn only_links_that_really_lead_into_a_set_count() {
        let set = set_name();
        let a = instance("a", &set, &[("resourcepacks/p.zip", "p")]);
        apply(&a).unwrap();
        let game = a.game_dir();
        assert!(is_share_link(&game.join("resourcepacks")));
        // Somewhere outside shared/, reachable from the game dir.
        let outside = format!("outside-{}", uuid::Uuid::new_v4().simple());
        let out_dir = paths::data_dir().join(&outside);
        std::fs::create_dir_all(out_dir.join("resourcepacks")).unwrap();
        std::fs::write(out_dir.join("resourcepacks/secret.txt"), "s").unwrap();
        let fake = |name: &str, target: String| {
            std::os::unix::fs::symlink(target, game.join(name)).unwrap();
            game.join(name)
        };
        // Starts like a share link but climbs back out.
        assert!(!is_share_link(&fake("loot", format!("{UP}/shared/../{outside}"))));
        assert!(!is_share_link(&fake("schematics2", format!("{UP}/shared/sets/{set}/../../../{outside}/resourcepacks"))));
        // A set's folder under another folder's name.
        std::fs::remove_dir_all(game.join("shaderpacks")).unwrap();
        assert!(!is_share_link(&fake("shaderpacks", format!("{UP}/shared/sets/{set}/resourcepacks"))));
        std::fs::remove_file(game.join("shaderpacks")).unwrap();

        // A set folder swapped for a link elsewhere: the right shape, the wrong place.
        let bad = set_name();
        std::fs::create_dir_all(sets_dir(Kind::Packs)).unwrap();
        std::os::unix::fs::symlink(&out_dir, set_dir(Kind::Packs, &bad)).unwrap();
        let mut b = instance("b", &bad, &[("resourcepacks/mine.zip", "m")]);
        assert!(apply(&b).is_err(), "moved files into a folder outside shared/");
        assert!(b.game_dir().join("resourcepacks/mine.zip").is_file());
        std::fs::remove_dir_all(b.game_dir().join("resourcepacks")).unwrap();
        std::os::unix::fs::symlink(format!("{UP}/shared/sets/{bad}/resourcepacks"), b.game_dir().join("resourcepacks")).unwrap();
        assert!(!is_share_link(&b.game_dir().join("resourcepacks")));
        // Leaving it doesn't copy the outside folder in either.
        b.share_set.clear();
        assert!(apply(&b).is_err());
        assert!(std::fs::symlink_metadata(b.game_dir().join("resourcepacks")).unwrap().is_symlink());

        std::fs::remove_file(set_dir(Kind::Packs, &bad)).unwrap();
        std::fs::remove_dir_all(out_dir).unwrap();
        for i in [&a, &b] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        delete_set(Kind::Packs, &set).unwrap();
    }

    #[test]
    fn an_interrupted_leave_is_finished_by_the_next_apply() {
        let set = set_name();
        let other = instance("other", &set, &[("shaderpacks/s.zip", "s")]);
        apply(&other).unwrap();
        let mut a = instance("a", &set, &[]);
        apply(&a).unwrap();
        a.share_set.clear();
        let link = a.game_dir().join("shaderpacks");
        let target = std::fs::read_link(&link).unwrap();
        let (copying, copied) = (staging(&link, "copying"), staging(&link, "copied"));
        let whole = |dir: &Path| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("s.zip"), "s").unwrap();
        };
        let check = || {
            assert!(!std::fs::symlink_metadata(&link).unwrap().is_symlink());
            assert_eq!(names(&link), ["s.zip"]);
            assert!(std::fs::symlink_metadata(&copying).is_err() && std::fs::symlink_metadata(&copied).is_err());
            // Back to a link for the next case.
            std::fs::remove_dir_all(&link).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
        };

        // Cut short while copying: the half-made copy goes, and the leave runs again.
        std::fs::create_dir_all(&copying).unwrap();
        std::fs::write(copying.join("half.zip"), "h").unwrap();
        apply(&a).unwrap();
        check();
        // Copied, but not swapped in yet.
        whole(&copied);
        apply(&a).unwrap();
        check();
        // Link removed, copy not yet renamed in (filesystems without an atomic swap).
        std::fs::remove_file(&link).unwrap();
        whole(&copied);
        apply(&a).unwrap();
        check();
        // Swapped, but the old link not yet removed.
        std::fs::remove_file(&link).unwrap();
        whole(&link);
        std::os::unix::fs::symlink(&target, &copied).unwrap();
        apply(&a).unwrap();
        assert!(std::fs::symlink_metadata(&copied).is_err());
        assert_eq!(names(&link), ["s.zip"]);
        // The set itself was never touched.
        assert_eq!(names(&set_dir(Kind::Packs, &set).join("shaderpacks")), ["s.zip"]);

        // A copy left behind by an instance that has since joined again is dropped.
        a.share_set = set.clone();
        std::fs::remove_dir_all(&link).unwrap();
        whole(&copied);
        apply(&a).unwrap();
        assert!(is_share_link(&link) && std::fs::symlink_metadata(&copied).is_err());

        for i in [&a, &other] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        delete_set(Kind::Packs, &set).unwrap();
    }

    #[test]
    fn links_inside_a_shared_folder_become_copies() {
        let set = set_name();
        let a = instance("a", &set, &[("original.zip", "o"), ("packs-elsewhere/p/pack.mcmeta", "m"), ("schematics/h.litematic", "h")]);
        let game = a.game_dir();
        std::fs::create_dir_all(game.join("resourcepacks/nested")).unwrap();
        // Relative links, which would point somewhere else from shared/sets/<set>/.
        std::os::unix::fs::symlink("../original.zip", game.join("resourcepacks/link.zip")).unwrap();
        std::os::unix::fs::symlink("../../packs-elsewhere/p", game.join("resourcepacks/nested/p")).unwrap();
        let notes = apply(&a).unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        let shared = set_dir(Kind::Packs, &set).join("resourcepacks");
        assert!(!std::fs::symlink_metadata(shared.join("link.zip")).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(shared.join("link.zip")).unwrap(), "o");
        assert_eq!(std::fs::read_to_string(shared.join("nested/p/pack.mcmeta")).unwrap(), "m");
        // The originals stay where they were.
        assert!(game.join("original.zip").is_file() && game.join("packs-elsewhere/p/pack.mcmeta").is_file());

        // A broken link stops the join before anything moves.
        let b = instance("b", &set, &[("shaderpacks/s.zip", "s")]);
        std::os::unix::fs::symlink("../gone.zip", b.game_dir().join("shaderpacks/broken.zip")).unwrap();
        let err = format!("{:#}", apply(&b).unwrap_err());
        assert!(err.contains("broken.zip"), "{err}");
        assert!(b.game_dir().join("shaderpacks/s.zip").is_file());
        assert!(!std::fs::symlink_metadata(b.game_dir().join("shaderpacks")).unwrap().is_symlink());
        assert!(!set_dir(Kind::Packs, &set).join("shaderpacks/s.zip").exists());

        for i in [&a, &b] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        delete_set(Kind::Packs, &set).unwrap();
    }

    #[test]
    fn world_sets_share_saves_apart_from_share_sets() {
        let set = set_name();
        // A pack set and a world set of the same name stay apart.
        let mut a = instance("a", &set, &[("saves/Base/level.dat", "a"), ("saves/Clash/level.dat", "A"), ("resourcepacks/p.zip", "p")]);
        let mut b = instance("b", "", &[("saves/Clash/level.dat", "B"), ("saves/Clash/region/r.0.0.mca", "r")]);
        a.world_set = set.clone();
        b.world_set = set.clone();
        assert!(apply(&a).unwrap().is_empty());
        let notes = apply(&b).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");

        // Worlds clash whole, never merge: b's comes in renamed, with all its files.
        let saves = set_dir(Kind::Worlds, &set).join("saves");
        let clash = format!("Clash ({})", b.id);
        assert_eq!(names(&saves), ["Base".to_string(), "Clash".into(), clash.clone()]);
        assert_eq!(std::fs::read_to_string(saves.join("Clash/level.dat")).unwrap(), "A");
        assert_eq!(names(&saves.join(&clash)), ["level.dat", "region"]);
        for i in [&a, &b] {
            assert!(is_share_link(&i.saves_dir()));
            assert_eq!(names(&i.saves_dir()), names(&saves));
        }
        assert!(is_share_link(&a.game_dir().join("resourcepacks")) && set_dir(Kind::Packs, &set).join("resourcepacks/p.zip").is_file());
        let mut users = sets_in_use(Kind::Worlds).remove(&set).unwrap();
        users.sort();
        let mut both = vec![a.id.clone(), b.id.clone()];
        both.sort();
        assert_eq!(users, both);
        assert_eq!(sets_in_use(Kind::Packs).get(&set), Some(&vec![a.id.clone()]));

        // Leaving gives b its own copy of every world in the set.
        b.world_set.clear();
        apply(&b).unwrap();
        assert!(!std::fs::symlink_metadata(b.saves_dir()).unwrap().is_symlink());
        assert_eq!(names(&b.saves_dir()), names(&saves));
        assert!(delete_set(Kind::Worlds, &set).is_err(), "a still links in");
        for i in [&a, &b] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        delete_set(Kind::Worlds, &set).unwrap();
        delete_set(Kind::Packs, &set).unwrap();
    }

    #[test]
    fn moving_to_another_set_relinks() {
        let (one, two) = (set_name(), set_name());
        let mut a = instance("a", &one, &[("resourcepacks/p.zip", "p")]);
        apply(&a).unwrap();
        a.share_set = two.clone();
        apply(&a).unwrap();
        assert_eq!(std::fs::read_link(a.game_dir().join("resourcepacks")).unwrap(), PathBuf::from(format!("{UP}/shared/sets/{two}/resourcepacks")));
        assert!(names(&a.game_dir().join("resourcepacks")).is_empty());
        assert_eq!(names(&set_dir(Kind::Packs, &one).join("resourcepacks")), ["p.zip"]);
        std::fs::remove_dir_all(a.dir()).unwrap();
        delete_set(Kind::Packs, &one).unwrap();
        delete_set(Kind::Packs, &two).unwrap();
    }

    #[test]
    fn own_links_stay() {
        let a = instance("links", "", &[]);
        let elsewhere = paths::test_root().join(format!("own-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, a.game_dir().join("resourcepacks")).unwrap();
        let mut a2 = a.clone();
        a2.share_set = set_name();
        let notes = apply(&a2).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(std::fs::read_link(a.game_dir().join("resourcepacks")).unwrap(), elsewhere);
        assert!(is_share_link(&a.game_dir().join("schematics")));
        std::fs::remove_dir_all(a.dir()).unwrap();
        std::fs::remove_dir_all(elsewhere).unwrap();
        delete_set(Kind::Packs, &a2.share_set).unwrap();
    }

    #[test]
    fn clash_names_keep_extensions() {
        let d = paths::test_root().join(format!("names-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(free_name(&d, "pack.zip", "x"), d.join("pack (x).zip"));
        assert_eq!(free_name(&d, "pack.zip.disabled", "x"), d.join("pack (x).zip.disabled"));
        assert_eq!(free_name(&d, ".hidden", "x"), d.join(".hidden (x)"));
        std::fs::write(d.join("pack (x).zip"), "").unwrap();
        assert_eq!(free_name(&d, "pack.zip", "x"), d.join("pack (x 2).zip"));
        assert!(check_set_name("../up").is_err() && check_set_name(".hidden").is_err() && check_set_name("1.21 packs").is_ok());
        std::fs::remove_dir_all(d).unwrap();
    }
}
