//! Shared folders. Minecraft has no setting for where screenshots or packs live,
//! so sharing turns the instance's folder into a link to one under `shared/`:
//!
//! - `shared/screenshots`: every instance's screenshots, when the setting is on.
//! - `shared/sets/<name>/{resourcepacks,shaderpacks,schematics}`: one pool per share
//!   set, a named set of instances (packs are tied to versions, so one global pool
//!   fills up with packs half the instances can't use).
//!
//! `apply` makes an instance's folders match its settings. Sharing moves the
//! instance's files into the shared folder (a name clash keeps both and renames the
//! incoming one; identical files are kept once). Unsharing leaves the instance a full
//! copy (reflinks, so free on btrfs). Links are relative, so the data dir can move.
//!
//! Screenshots are different: Omacraft records which instance each one came from
//! (`shared/screenshots.owners.toml`), from the game's "Saved screenshot as" log line,
//! so turning sharing off moves each instance's own screenshots back instead of
//! copying the whole pool into every instance.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::instance::Instance;
use crate::{fsutil, paths};

/// Folders a share set shares.
pub const SET_FOLDERS: [&str; 3] = ["resourcepacks", "shaderpacks", "schematics"];
pub const SCREENSHOTS: &str = "screenshots";

/// From an instance's game dir (`instances/<id>/minecraft`) back to the data dir.
const UP: &str = "../../..";

/// One `apply` at a time, so two never move the same files.
static LOCK: Mutex<()> = Mutex::new(());

pub fn dir() -> PathBuf {
    paths::data_dir().join("shared")
}

pub fn sets_dir() -> PathBuf {
    dir().join("sets")
}

pub fn set_dir(name: &str) -> PathBuf {
    sets_dir().join(name)
}

/// A share set's name doubles as its folder name.
pub fn check_set_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) || name.chars().any(char::is_control) || name.len() > 64 {
        bail!("“{name}” can't be a share set name: use up to 64 characters, no slashes, not starting with a dot");
    }
    Ok(())
}

/// Share sets with a folder on disk, sorted. Reads one directory.
pub fn sets_on_disk() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(sets_dir())
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

/// Delete a share set's folder and everything in it.
pub fn delete_set(name: &str) -> anyhow::Result<()> {
    check_set_name(name)?;
    std::fs::remove_dir_all(set_dir(name)).with_context(|| format!("deleting share set {name}"))
}

/// Where `folder` in the game dir should link to, relative to the game dir.
fn target(inst: &Instance, screenshots: bool, folder: &str) -> Option<PathBuf> {
    if folder == SCREENSHOTS {
        screenshots.then(|| PathBuf::from(format!("{UP}/shared/{SCREENSHOTS}")))
    } else {
        (!inst.share_set.is_empty()).then(|| PathBuf::from(format!("{UP}/shared/sets/{}/{folder}", inst.share_set)))
    }
}

/// A link Omacraft made: relative into `shared/`, or (made elsewhere) absolute into it.
fn is_ours(target: &Path) -> bool {
    target.starts_with(format!("{UP}/shared")) || (target.is_absolute() && target.starts_with(dir()))
}

/// Whether `path` is a link to a shared folder (export follows these).
pub fn is_share_link(path: &Path) -> bool {
    std::fs::read_link(path).is_ok_and(|t| is_ours(&t))
}

/// Make the instance's folders match its settings: linked to the shared folders
/// it uses, real folders for the rest. Returns notes (renamed clashes, skipped
/// links) for the user. Safe to run again after an interruption.
pub fn apply(inst: &Instance, screenshots: bool) -> anyhow::Result<Vec<String>> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !inst.share_set.is_empty() {
        check_set_name(&inst.share_set)?;
    }
    let game = inst.game_dir();
    std::fs::create_dir_all(&game)?;
    let mut notes = Vec::new();
    for folder in std::iter::once(SCREENSHOTS).chain(SET_FOLDERS) {
        let link = game.join(folder);
        let want = target(inst, screenshots, folder);
        let current = std::fs::read_link(&link).ok();
        match (want, current) {
            (Some(w), Some(c)) if w == c => std::fs::create_dir_all(game.join(&w))?,
            // Moved to another share set: the files stay with the old one.
            (Some(w), Some(c)) if is_ours(&c) => {
                std::fs::create_dir_all(game.join(&w))?;
                link_to(&link, &w)?;
            }
            (None, Some(c)) if is_ours(&c) && folder == SCREENSHOTS => take_screenshots_back(&link, &inst.id, &mut notes).context("unsharing screenshots")?,
            (None, Some(c)) if is_ours(&c) => unshare(&link, &mut notes).with_context(|| format!("unsharing {folder}"))?,
            (Some(_), Some(_)) => notes.push(format!("{}: left {folder} alone, it's a link Omacraft didn't make", inst.name)),
            (Some(w), None) => {
                let shared = game.join(&w);
                std::fs::create_dir_all(&shared)?;
                match std::fs::symlink_metadata(&link) {
                    Ok(m) if m.is_dir() && folder == SCREENSHOTS => {
                        move_screenshots_in(&link, &shared, &inst.id, &mut notes).context("moving screenshots into the shared folder")?;
                    }
                    Ok(m) if m.is_dir() => {
                        // Pack folders (unzipped packs) clash as a whole; other folders merge.
                        let merge = folder != "resourcepacks" && folder != "shaderpacks";
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
            // Finishes a take-back that was interrupted.
            (None, None) if folder == SCREENSHOTS => take_screenshots_back(&link, &inst.id, &mut notes).context("unsharing screenshots")?,
            (None, _) => {}
        }
    }
    Ok(notes)
}

// -- screenshot owners ----------------------------------------------------------------

fn shots_dir() -> PathBuf {
    dir().join(SCREENSHOTS)
}

fn owners_file() -> PathBuf {
    dir().join("screenshots.owners.toml")
}

/// Shared screenshot file name → id of the instance that took it.
#[derive(Default, Serialize, Deserialize)]
struct Owners {
    #[serde(default)]
    owners: BTreeMap<String, String>,
}

impl Owners {
    fn load() -> Self {
        std::fs::read_to_string(owners_file()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    /// Save, forgetting screenshots that were deleted.
    fn save(&mut self) -> anyhow::Result<()> {
        let shots = shots_dir();
        self.owners.retain(|name, _| std::fs::symlink_metadata(shots.join(name)).is_ok());
        let text = toml::to_string_pretty(self)?;
        fsutil::write_atomic(&owners_file(), text.as_bytes(), 0o644)?;
        Ok(())
    }
}

/// Move an instance's screenshots into the shared folder, recording them as its own.
fn move_screenshots_in(src: &Path, shared: &Path, id: &str, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let mut owners = Owners::load();
    let r = (|| {
        for e in std::fs::read_dir(src)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let from = e.path();
            let mut to = shared.join(&name);
            if let Ok(m) = std::fs::symlink_metadata(&to) {
                if m.is_file() && e.file_type()?.is_file() && same_bytes(&from, &to)? {
                    std::fs::remove_file(&from)?;
                    owners.owners.entry(name).or_insert_with(|| id.to_string());
                    continue;
                }
                to = free_name(shared, &name, id);
                notes.push(format!("{name} was in both; this instance's copy is now {}", to.file_name().unwrap_or_default().to_string_lossy()));
            }
            move_entry(&from, &to)?;
            owners.owners.insert(to.file_name().unwrap_or_default().to_string_lossy().into_owned(), id.to_string());
        }
        std::fs::remove_dir(src)?;
        anyhow::Ok(())
    })();
    owners.save()?;
    r
}

/// Turn `link` back into a real folder holding the screenshots this instance took.
/// The rest stay shared. Moves, never copies, so no filesystem pays twice.
fn take_screenshots_back(link: &Path, id: &str, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let mut owners = Owners::load();
    let mine: Vec<String> = owners.owners.iter().filter(|(_, o)| *o == id).map(|(n, _)| n.clone()).collect();
    let is_link = std::fs::read_link(link).is_ok();
    if !is_link && mine.is_empty() {
        return Ok(());
    }
    if is_link {
        std::fs::remove_file(link)?;
    }
    std::fs::create_dir_all(link)?;
    let shared = shots_dir();
    let r = (|| {
        for name in mine {
            let from = shared.join(&name);
            if std::fs::symlink_metadata(&from).is_err() {
                continue;
            }
            let mut to = link.join(&name);
            if std::fs::symlink_metadata(&to).is_ok() {
                to = free_name(link, &name, "shared");
                notes.push(format!("{name} was in both; the shared one is now {}", to.file_name().unwrap_or_default().to_string_lossy()));
            }
            move_entry(&from, &to)?;
            owners.owners.remove(&name);
        }
        anyhow::Ok(())
    })();
    owners.save()?;
    r
}

/// Screenshot names in the shared folder that `id` took.
pub fn owned_screenshots(id: &str) -> HashSet<String> {
    Owners::load().owners.into_iter().filter(|(_, o)| o == id).map(|(n, _)| n).collect()
}

/// Delete the shared screenshots a deleted instance took, as deleting it would have
/// before they were shared.
pub fn delete_screenshots_of(id: &str) -> anyhow::Result<()> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut owners = Owners::load();
    for name in owners.owners.iter().filter(|(_, o)| *o == id).map(|(n, _)| n) {
        let p = shots_dir().join(name);
        let _ = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    }
    owners.save()
}

/// Shared screenshots no existing instance owns (dropped in by hand, or from a
/// session that couldn't be told apart). They stay in the shared folder.
pub fn untraced_screenshots() -> usize {
    let owners = Owners::load().owners;
    std::fs::read_dir(shots_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter(|e| owners.get(&*e.file_name().to_string_lossy()).is_none_or(|id| !paths::instances_dir().join(id).is_dir()))
        .count()
}

static SESSIONS: Mutex<Vec<(u64, bool)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A game session, for telling whose screenshots are whose. The log names each
/// screenshot; when it doesn't, new files are claimed by time, but only if no other
/// game ran alongside.
pub struct Session {
    token: u64,
    since: SystemTime,
}

impl Session {
    pub fn begin() -> Self {
        let token = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut v = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let overlapped = !v.is_empty();
        v.iter_mut().for_each(|s| s.1 = true);
        v.push((token, overlapped));
        Session { token, since: SystemTime::now() }
    }

    /// Record the screenshots this session took as `id`'s, if its screenshots are shared.
    pub fn end(self, inst: &Instance, logged: &[String]) -> anyhow::Result<()> {
        let alone = !SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|s| s.0 == self.token && s.1);
        if !is_share_link(&inst.game_dir().join(SCREENSHOTS)) {
            return Ok(());
        }
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut owners = Owners::load();
        let shots = shots_dir();
        for name in logged {
            if shots.join(name).is_file() {
                owners.owners.entry(name.clone()).or_insert_with(|| inst.id.clone());
            }
        }
        if alone {
            for e in std::fs::read_dir(&shots).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let new = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= self.since);
                if new && !name.starts_with('.') {
                    owners.owners.entry(name).or_insert_with(|| inst.id.clone());
                }
            }
        }
        owners.save()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).retain(|s| s.0 != self.token);
    }
}

/// Point `link` at `target`, replacing whatever link is there in one step.
fn link_to(link: &Path, target: &Path) -> std::io::Result<()> {
    let tmp = link.with_file_name(format!(".{}.{}.link", link.file_name().unwrap_or_default().to_string_lossy(), uuid::Uuid::new_v4().simple()));
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Replace a shared-folder link with a full copy of what it points to.
fn unshare(link: &Path, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let name = link.file_name().unwrap_or_default().to_string_lossy();
    let tmp = link.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let copied = match link.canonicalize() {
        Ok(src) => fsutil::copy_tree(&src, &tmp, &|_| {}),
        // The shared folder is gone; start empty.
        Err(_) => std::fs::create_dir(&tmp).map(|_| Vec::new()),
    };
    match copied {
        Ok(skipped) => notes.extend(skipped),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e.into());
        }
    }
    std::fs::remove_file(link)?;
    std::fs::rename(&tmp, link)?;
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

/// Rename, or copy and delete across filesystems.
fn move_entry(from: &Path, to: &Path) -> anyhow::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            if std::fs::symlink_metadata(from)?.is_dir() {
                fsutil::copy_tree(from, to, &|_| {})?;
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

/// The shared screenshots folder is global, so tests that use it take turns.
#[cfg(test)]
pub static TEST_SHOTS: Mutex<()> = Mutex::new(());

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
        assert!(apply(&a, false).unwrap().is_empty());
        let notes = apply(&b, false).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");

        let shared = set_dir(&set).join("resourcepacks");
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
            // Screenshots aren't shared unless the setting is on.
            assert!(std::fs::read_link(i.game_dir().join(SCREENSHOTS)).is_err());
        }
        // Running it again changes nothing.
        assert!(apply(&b, false).unwrap().is_empty());
        assert_eq!(names(&shared), want);

        // Deleting an instance removes its link, never the shared files.
        std::fs::remove_dir_all(b.dir()).unwrap();
        assert_eq!(names(&shared), want);
        std::fs::remove_dir_all(a.dir()).unwrap();
        delete_set(&set).unwrap();
    }

    #[test]
    fn unsharing_leaves_a_full_copy() {
        let set = set_name();
        let mut a = instance("a", &set, &[("shaderpacks/s.zip", "s")]);
        let b = instance("b", &set, &[]);
        apply(&a, false).unwrap();
        apply(&b, false).unwrap();
        a.share_set.clear();
        apply(&a, false).unwrap();
        let own = a.game_dir().join("shaderpacks");
        assert!(!std::fs::symlink_metadata(&own).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(own.join("s.zip")).unwrap(), "s");
        // The copy is independent: removing it leaves the set's file alone.
        std::fs::remove_file(own.join("s.zip")).unwrap();
        assert_eq!(names(&b.game_dir().join("shaderpacks")), ["s.zip"]);
        for i in [&a, &b] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        delete_set(&set).unwrap();
    }

    #[test]
    fn moving_to_another_set_relinks() {
        let (one, two) = (set_name(), set_name());
        let mut a = instance("a", &one, &[("resourcepacks/p.zip", "p")]);
        apply(&a, false).unwrap();
        a.share_set = two.clone();
        apply(&a, false).unwrap();
        assert_eq!(std::fs::read_link(a.game_dir().join("resourcepacks")).unwrap(), PathBuf::from(format!("{UP}/shared/sets/{two}/resourcepacks")));
        assert!(names(&a.game_dir().join("resourcepacks")).is_empty());
        assert_eq!(names(&set_dir(&one).join("resourcepacks")), ["p.zip"]);
        std::fs::remove_dir_all(a.dir()).unwrap();
        delete_set(&one).unwrap();
        delete_set(&two).unwrap();
    }


    #[test]
    fn unsharing_screenshots_gives_each_its_own_back() {
        let _g = TEST_SHOTS.lock().unwrap_or_else(|e| e.into_inner());
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let shot = |n: &str| format!("screenshots/{tag}-{n}.png");
        let a = instance("a", "", &[(&shot("a"), "a"), (&shot("both"), "A")]);
        let b = instance("b", "", &[(&shot("b"), "b"), (&shot("both"), "B")]);
        apply(&a, true).unwrap();
        assert_eq!(apply(&b, true).unwrap().len(), 1);
        // A game session: one screenshot named in the log, one found by time.
        let session = Session::begin();
        std::fs::write(a.screenshots_dir().join(format!("{tag}-logged.png")), "l").unwrap();
        let timed = a.screenshots_dir().join(format!("{tag}-timed.png"));
        std::fs::write(&timed, "t").unwrap();
        // File times come from a coarse clock that can lag the session's start.
        std::fs::File::options().write(true).open(&timed).unwrap().set_modified(SystemTime::now() + std::time::Duration::from_secs(1)).unwrap();
        session.end(&a, &[format!("{tag}-logged.png")]).unwrap();
        // Both see every screenshot while shared.
        assert_eq!(names(&b.screenshots_dir()).iter().filter(|n| n.starts_with(&tag)).count(), 6);

        apply(&a, false).unwrap();
        let mine = |i: &Instance| names(&i.screenshots_dir()).into_iter().filter(|n| n.starts_with(&tag)).collect::<Vec<_>>();
        assert!(!std::fs::symlink_metadata(a.screenshots_dir()).unwrap().is_symlink());
        let want_a: Vec<String> = ["a", "both", "logged", "timed"].iter().map(|n| format!("{tag}-{n}.png")).collect();
        assert_eq!(mine(&a), want_a);
        // Moved, not copied: b keeps only its own.
        let both_b = format!("{tag}-both ({}).png", b.id);
        assert_eq!(mine(&b), [both_b.clone(), format!("{tag}-b.png")].into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>());
        apply(&b, false).unwrap();
        assert_eq!(std::fs::read_to_string(b.screenshots_dir().join(&both_b)).unwrap(), "B");
        assert!(names(&shots_dir()).iter().all(|n| !n.starts_with(&tag)));
        for i in [&a, &b] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
    }

    #[test]
    fn overlapping_sessions_claim_only_logged_screenshots() {
        let _g = TEST_SHOTS.lock().unwrap_or_else(|e| e.into_inner());
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let a = instance("a", "", &[]);
        apply(&a, true).unwrap();
        let (one, two) = (Session::begin(), Session::begin());
        std::fs::write(shots_dir().join(format!("{tag}-x.png")), "x").unwrap();
        one.end(&a, &[]).unwrap();
        drop(two);
        assert!(owned_screenshots(&a.id).is_empty(), "claimed a screenshot another game may have taken");
        assert!(untraced_screenshots() >= 1);
        std::fs::remove_file(shots_dir().join(format!("{tag}-x.png"))).unwrap();
        delete_screenshots_of(&a.id).unwrap();
        std::fs::remove_dir_all(a.dir()).unwrap();
    }

    #[test]
    fn screenshots_share_globally_and_own_links_stay() {
        let _g = TEST_SHOTS.lock().unwrap_or_else(|e| e.into_inner());
        let a = instance("shots", "", &[("screenshots/2026-10-17_12.00.00.png", "png")]);
        let elsewhere = paths::test_root().join(format!("own-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, a.game_dir().join("resourcepacks")).unwrap();
        let mut a2 = a.clone();
        a2.share_set = set_name();
        let notes = apply(&a2, true).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(std::fs::read_link(a.game_dir().join("resourcepacks")).unwrap(), elsewhere);
        assert!(dir().join(SCREENSHOTS).join("2026-10-17_12.00.00.png").is_file());
        assert!(is_share_link(&a.game_dir().join(SCREENSHOTS)));
        // Deleting the instance takes its screenshots with it, as before sharing.
        std::fs::remove_dir_all(a.dir()).unwrap();
        delete_screenshots_of(&a.id).unwrap();
        assert!(!dir().join(SCREENSHOTS).join("2026-10-17_12.00.00.png").exists());
        std::fs::remove_dir_all(elsewhere).unwrap();
        delete_set(&a2.share_set).unwrap();
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
