//! Snapshots: reflink copies under `backups/`, free on btrfs and xfs.
//!
//! - Worlds go under `backups/worlds/`: a world set's under `sets/<set>/<world folder>/`,
//!   an instance's own under `instances/<id>/<world folder>/`. They're taken when a game
//!   exits (automatic, the oldest pruned past `Config::backups_keep`), by the pre-launch
//!   world check, before a restore, and by hand.
//! - An instance's `mods` and `config` go under `backups/instances/<id>/`, before mods
//!   are updated or rolled back. Rolling back puts both back as they were.
//!
//! Each snapshot is a folder named for when it was taken (UTC), next to a `<stamp>.toml`
//! saying why. It's copied under a dot-name and renamed in once whole, so a snapshot
//! that's listed is complete. Restores copy into a dot-name beside the snapshots and
//! swap in with a journal (`fsutil::swap_in`), so a crash never leaves a half-restored
//! world or mods folder, and [`recover`] finishes a swap cut short.
//!
//! Each folder of snapshots (a shelf) has a lock file beside it (`.<shelf>.lock`, an
//! `flock`), held through every snapshot, restore, delete and prune there, by any
//! process. So jobs from two members of a world set queue up instead of clearing
//! away each other's half-made copies.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instance::Instance;
use crate::{fsutil, paths};

/// Whose world a snapshot is.
#[derive(Debug, Clone, Copy)]
pub enum Owner<'a> {
    Set(&'a str),
    Instance(&'a str),
}

impl<'a> Owner<'a> {
    /// Where `inst`'s worlds' snapshots go: its world set's, or its own.
    pub fn of(inst: &'a Instance) -> Self {
        if inst.world_set.is_empty() { Owner::Instance(&inst.id) } else { Owner::Set(&inst.world_set) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub taken: DateTime<Utc>,
    pub reason: String,
    /// The folder it was copied from.
    pub source: PathBuf,
    /// Taken on its own (after a game exits, before an update), so pruned once there
    /// are more than the setting keeps. Ones taken by hand or by the world check stay.
    #[serde(default)]
    pub auto: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub info: Info,
}

impl Snapshot {
    /// When it was taken, in local time.
    pub fn when(&self) -> String {
        self.info.taken.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()
    }
}

pub fn root() -> PathBuf {
    paths::data_dir().join("backups")
}

pub fn dir() -> PathBuf {
    root().join("worlds")
}

fn owner_dir(owner: Owner) -> PathBuf {
    match owner {
        Owner::Set(s) => dir().join("sets").join(s),
        Owner::Instance(i) => dir().join("instances").join(i),
    }
}

/// Where snapshots of world folder `world` are kept.
pub fn world_dir(owner: Owner, world: &str) -> PathBuf {
    owner_dir(owner).join(world)
}

/// Where snapshots of instance `id`'s mods and config are kept.
pub fn instance_dir(id: &str) -> PathBuf {
    root().join("instances").join(id)
}

/// The lock file for `shelf`, beside it (so an emptied shelf can still be removed).
fn lock_path(shelf: &Path) -> PathBuf {
    shelf.with_file_name(format!(".{}.lock", shelf.file_name().unwrap_or_default().to_string_lossy()))
}

/// Take `shelf`'s lock, waiting for whatever job holds it.
fn lock(shelf: &Path) -> anyhow::Result<fsutil::FileLock> {
    fsutil::lock(&lock_path(shelf)).with_context(|| format!("locking {}", shelf.display()))
}

/// Take `shelf`'s lock, or fail soon if a job holds it (for the UI thread). It tries
/// for up to 200 ms: a process forked just as a job let go (a game being launched)
/// holds the job's lock until it execs.
fn try_lock(shelf: &Path) -> anyhow::Result<fsutil::FileLock> {
    lock_soon(shelf)?.context("a backup or restore of it is running; try again once it's done")
}

fn lock_soon(shelf: &Path) -> anyhow::Result<Option<fsutil::FileLock>> {
    for _ in 0..20 {
        if let Some(l) = fsutil::try_lock(&lock_path(shelf)).with_context(|| format!("locking {}", shelf.display()))? {
            return Ok(Some(l));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(None)
}

/// Take a snapshot into `shelf` (a folder of snapshots), whose lock the caller holds:
/// `fill` copies into the folder it's given. Returns the snapshot's folder.
fn take(shelf: &Path, source: &Path, reason: &str, auto: bool, fill: impl FnOnce(&Path) -> anyhow::Result<()>) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(shelf)?;
    // With the lock held, nothing is filling these: they're copies a crash cut short.
    for e in std::fs::read_dir(shelf)?.flatten() {
        if e.file_name().to_string_lossy().ends_with(".partial") {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let taken = Utc::now();
    let base = taken.format("%Y-%m-%dT%H-%M-%S").to_string();
    let stamp = (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|s| std::fs::symlink_metadata(shelf.join(s)).is_err() && std::fs::symlink_metadata(shelf.join(format!("{s}.toml"))).is_err())
        .unwrap();
    let partial = shelf.join(format!(".{stamp}.{}.partial", uuid::Uuid::new_v4().simple()));
    if let Err(e) = fill(&partial) {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }
    // The real folder, not the path through an instance's share link.
    let source = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    let info = Info { taken, reason: reason.to_string(), source, auto };
    fsutil::write_atomic(&shelf.join(format!("{stamp}.toml")), toml::to_string_pretty(&info)?.as_bytes(), 0o644)?;
    let done = shelf.join(&stamp);
    std::fs::rename(&partial, &done)?;
    Ok(done)
}

/// The finished snapshots in `shelf`, newest first.
fn list(shelf: &Path) -> Vec<Snapshot> {
    let mut out: Vec<Snapshot> = std::fs::read_dir(shelf)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.') && e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let text = std::fs::read_to_string(shelf.join(format!("{}.toml", e.file_name().to_string_lossy()))).ok()?;
            Some(Snapshot { dir: e.path(), info: toml::from_str(&text).ok()? })
        })
        .collect();
    out.sort_by(|a, b| b.info.taken.cmp(&a.info.taken));
    out
}

/// Delete a snapshot. It stops being listed first, and a crash partway leaves a
/// dot-folder the next snapshot there clears away. Fails at once while another job
/// works on its shelf.
pub fn delete(s: &Snapshot) -> anyhow::Result<()> {
    let shelf = s.dir.parent().context("snapshot has no parent folder")?;
    let _l = try_lock(shelf)?;
    delete_locked(s)
}

fn delete_locked(s: &Snapshot) -> anyhow::Result<()> {
    let shelf = s.dir.parent().context("snapshot has no parent folder")?;
    let stamp = s.dir.file_name().context("snapshot has no name")?.to_string_lossy().into_owned();
    let gone = shelf.join(format!(".{stamp}.deleting.partial"));
    std::fs::rename(&s.dir, &gone)?;
    let _ = std::fs::remove_file(shelf.join(format!("{stamp}.toml")));
    std::fs::remove_dir_all(&gone)?;
    // An emptied world's shelf goes too, so a deleted world stops showing up.
    if std::fs::read_dir(shelf).is_ok_and(|mut d| d.next().is_none()) {
        let _ = std::fs::remove_dir(shelf);
    }
    Ok(())
}

/// Delete the oldest automatic snapshots in `shelf` past the newest `keep`. The caller
/// holds its lock.
fn prune(shelf: &Path, keep: usize) -> Vec<String> {
    list(shelf).iter().filter(|s| s.info.auto).skip(keep).filter_map(|s| delete_locked(s).err().map(|e| format!("couldn't prune {}: {e:#}", s.dir.display()))).collect()
}

/// Snapshot the world at `world` (a folder in some `saves/`). Returns the snapshot's folder.
pub fn snapshot_world(world: &Path, owner: Owner, reason: &str, auto: bool) -> anyhow::Result<PathBuf> {
    let folder = world.file_name().context("world has no folder name")?.to_string_lossy().into_owned();
    let shelf = world_dir(owner, &folder);
    let _l = lock(&shelf)?;
    copy_world(world, &shelf, reason, auto)
}

/// [`snapshot_world`] with `shelf`'s lock already held.
fn copy_world(world: &Path, shelf: &Path, reason: &str, auto: bool) -> anyhow::Result<PathBuf> {
    take(shelf, world, reason, auto, |into| {
        fsutil::copy_tree(world, into, &|_| {}).with_context(|| format!("copying {}", world.display()))?;
        // The running game's lock, which means nothing in a copy.
        let _ = std::fs::remove_file(into.join("session.lock"));
        Ok(())
    })
}

/// The automatic snapshot taken after a game exits, then the oldest automatic ones
/// past `keep` pruned. Skipped if the world is open in another game. Returns warnings.
pub fn after_play(world: &Path, owner: Owner, inst_name: &str, keep: u32) -> Vec<String> {
    if keep == 0 || crate::world_sets::lock_holder(world).is_some() {
        return Vec::new();
    }
    let name = world.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let shelf = world_dir(owner, &name);
    let r = lock(&shelf).and_then(|_l| copy_world(world, &shelf, &format!("after {inst_name} played it"), true).map(|_| prune(&shelf, keep as usize)));
    r.unwrap_or_else(|e| vec![format!("couldn't back up {name}: {e:#}")])
}

/// Snapshot every world in `inst`'s saves that no snapshot covers since it was last
/// changed, so a rollback has a version from before now to go back to. Skips worlds
/// open in a game; fails if any other world couldn't be backed up.
pub fn cover_worlds(inst: &Instance, reason: &str) -> anyhow::Result<()> {
    let owner = Owner::of(inst);
    let mut failed = Vec::new();
    for (folder, dir) in every_world(&inst.saves_dir())? {
        let modified = std::fs::metadata(dir.join("level.dat")).and_then(|m| m.modified()).ok().map(DateTime::<Utc>::from);
        let covered = snapshots(owner, &folder).iter().any(|s| modified.is_some_and(|m| s.info.taken >= m));
        if covered || crate::world_sets::lock_holder(&dir).is_some() {
            continue;
        }
        if let Err(e) = snapshot_world(&dir, owner, reason, false) {
            failed.push(format!("{folder}: {e:#}"));
        }
    }
    if !failed.is_empty() {
        bail!("couldn't back up {}", failed.join("; "));
    }
    Ok(())
}

/// Every world folder in `saves` (folder name, path), for backups that must not miss
/// one: unlike the worlds tab's listing, a folder that can't be read fails instead of
/// looking empty. No `saves` at all is no worlds.
fn every_world(saves: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(saves) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        r => r.with_context(|| format!("reading {}", saves.display()))?,
    };
    let mut out = Vec::new();
    for e in entries {
        let dir = e.with_context(|| format!("reading {}", saves.display()))?.path();
        match std::fs::metadata(dir.join("level.dat")) {
            Ok(m) if m.is_file() => out.push((dir.file_name().unwrap_or_default().to_string_lossy().into_owned(), dir)),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {}
            Err(e) => return Err(e).with_context(|| format!("looking into {}", dir.display())),
        }
    }
    Ok(out)
}

/// The finished snapshots of world folder `world`, newest first.
pub fn snapshots(owner: Owner, world: &str) -> Vec<Snapshot> {
    list(&world_dir(owner, world))
}

/// Every world folder with snapshots (whether or not the world is still there) → its snapshots.
pub fn all_worlds(owner: Owner) -> BTreeMap<String, Vec<Snapshot>> {
    std::fs::read_dir(owner_dir(owner))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), list(&e.path())))
        .filter(|(_, l)| !l.is_empty())
        .collect()
}

/// Put a world back as it was in snapshot `s`, into `saves`. The world as it is now
/// is snapshotted first, so a restore can be undone. Refuses while a game has it open.
pub fn restore_world(s: &Snapshot, saves: &Path) -> anyhow::Result<()> {
    let shelf = s.dir.parent().context("snapshot has no parent folder")?;
    let folder = shelf.file_name().context("snapshot has no world")?.to_string_lossy().into_owned();
    let _l = lock(shelf)?;
    recover_locked(shelf)?;
    std::fs::create_dir_all(saves)?;
    // The real folder, so the journal leads there from any member of a world set.
    let world = saves.canonicalize()?.join(&folder);
    let open = || crate::world_sets::lock_holder(&world).is_some();
    if open() {
        bail!("“{folder}” is open in a game; close it first");
    }
    if world.join("level.dat").is_file() {
        copy_world(&world, shelf, &format!("before restoring the backup from {}", s.when()), false).context("backing up the world as it is now")?;
    }
    // Staged beside the snapshots, not in saves/, where the game would list it.
    let staged = shelf.join(".restoring");
    stage(&s.dir, &staged).context("copying the backup")?;
    // Looked at again: a game may have opened it while the copy ran.
    if open() {
        let _ = std::fs::remove_dir_all(&staged);
        bail!("“{folder}” was opened in a game; close it first");
    }
    fsutil::swap_in(&staged, &world).with_context(|| format!("putting it in {}", world.display()))?;
    Ok(())
}

/// Copy `from` into the fresh folder `staged`, clearing it away again on failure.
fn stage(from: &Path, staged: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_dir_all(staged);
    fsutil::copy_tree(from, staged, &|_| {}).map(|_| ()).map_err(|e| {
        let _ = std::fs::remove_dir_all(staged);
        e.into()
    })
}

/// Finish what a crash cut short in `inst`'s shelves (its mods and config, and its
/// worlds'): restores and rollbacks halfway through being swapped in are completed,
/// half-made copies cleared. Run before launch, so a game never starts with a world or
/// the mods folder swapped out: it waits for jobs holding a shelf (another process's),
/// and fails if any shelf couldn't be put right.
pub fn recover(inst: &Instance) -> anyhow::Result<()> {
    let mut shelves = world_shelves(Owner::of(inst))?;
    shelves.push(instance_dir(&inst.id));
    for shelf in shelves.iter().filter(|s| s.is_dir()) {
        let _l = lock(shelf)?;
        recover_locked(shelf).with_context(|| format!("finishing an interrupted restore in {}", shelf.display()))?;
    }
    Ok(())
}

/// Best-effort [`recover`] for the world shelves of `owner`, for the worlds tab: shelves
/// a job holds are left to it. Returns what went wrong, for the log.
pub fn recover_worlds(owner: Owner) -> Vec<String> {
    let shelves = match world_shelves(owner) {
        Ok(s) => s,
        Err(e) => return vec![format!("{e:#}")],
    };
    let recover = |shelf: &Path| match lock_soon(shelf)? {
        Some(_l) => recover_locked(shelf),
        None => Ok(()),
    };
    shelves.iter().filter(|s| s.is_dir()).filter_map(|s| recover(s).err().map(|e| format!("couldn't finish restoring {}: {e:#}", s.display()))).collect()
}

/// Every world shelf of `owner`: each folder, and each lock file whose folder a job may
/// be about to make.
fn world_shelves(owner: Owner) -> anyhow::Result<Vec<PathBuf>> {
    let dir = owner_dir(owner);
    let entries = match std::fs::read_dir(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        r => r.with_context(|| format!("reading {}", dir.display()))?,
    };
    let mut out = BTreeSet::new();
    for e in entries {
        let e = e.with_context(|| format!("reading {}", dir.display()))?;
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(shelf) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".lock")) {
            out.insert(dir.join(shelf));
        } else if e.file_type().is_ok_and(|t| t.is_dir()) {
            out.insert(e.path());
        }
    }
    Ok(out.into_iter().collect())
}

/// [`recover_shelf`] with the lock held: every journalled swap is finished; restore
/// copies without a journal never finished copying, and go.
fn recover_locked(shelf: &Path) -> anyhow::Result<()> {
    let names: Vec<String> = std::fs::read_dir(shelf)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    for n in names.iter().filter_map(|n| n.strip_suffix(".swap")) {
        let staged = shelf.join(n);
        fsutil::finish_swap(&staged).with_context(|| format!("finishing {}", staged.display()))?;
    }
    for n in names.iter().filter(|n| n.ends_with(".restoring")) {
        let p = shelf.join(n);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) && !fsutil::swap_journal(&p).exists() {
            std::fs::remove_dir_all(&p)?;
        }
    }
    Ok(())
}

/// What an instance snapshot holds.
pub const INSTANCE_FOLDERS: [&str; 2] = ["mods", "config"];

/// Snapshot `inst`'s mods and config, then prune its snapshots to `keep` (at least one,
/// this one). Returns the snapshot's folder.
pub fn snapshot_instance(inst: &Instance, reason: &str, keep: u32) -> anyhow::Result<PathBuf> {
    let _l = lock(&instance_dir(&inst.id))?;
    let done = take_instance(inst, reason)?;
    prune_instance(&inst.id, keep);
    Ok(done)
}

/// Snapshot `inst`'s mods and config, the shelf's lock held.
fn take_instance(inst: &Instance, reason: &str) -> anyhow::Result<PathBuf> {
    let game = inst.game_dir();
    take(&instance_dir(&inst.id), &game, reason, true, |into| {
        std::fs::create_dir_all(into)?;
        for f in INSTANCE_FOLDERS {
            let from = game.join(f);
            if from.is_dir() {
                fsutil::copy_tree(&from, &into.join(f), &|_| {}).with_context(|| format!("copying {}", from.display()))?;
            } else if f == "mods" {
                // No mods yet: rolling back to this removes the ones added since.
                std::fs::create_dir(into.join(f))?;
            }
        }
        Ok(())
    })
}

fn prune_instance(id: &str, keep: u32) {
    for e in prune(&instance_dir(id), keep.max(1) as usize) {
        eprintln!("{e}");
    }
}

/// `inst`'s snapshots of its mods and config, newest first.
pub fn instance_snapshots(id: &str) -> Vec<Snapshot> {
    list(&instance_dir(id))
}

/// Put `inst`'s mods (and config, if the snapshot has it) back as they were in `s`.
/// The current ones are snapshotted first, so a rollback can be undone.
pub fn roll_back(inst: &Instance, s: &Snapshot, keep: u32) -> anyhow::Result<()> {
    let shelf = instance_dir(&inst.id);
    let _l = lock(&shelf)?;
    recover_locked(&shelf)?;
    // Pruned only once done: `s` may be the oldest.
    take_instance(inst, &format!("before rolling back to {}", s.when())).context("backing up the mods as they are now")?;
    let game = inst.game_dir();
    for f in INSTANCE_FOLDERS {
        let from = s.dir.join(f);
        if !from.is_dir() {
            continue;
        }
        let staged = shelf.join(format!(".{f}.restoring"));
        stage(&from, &staged).with_context(|| format!("copying the backed up {f}"))?;
        fsutil::swap_in(&staged, &game.join(f)).with_context(|| format!("putting {f} back"))?;
    }
    prune_instance(&inst.id, keep);
    Ok(())
}

/// Every backup shelf of an instance being deleted, locked: no job (in any process) can
/// be using them. [`Self::forget`] deletes them.
pub struct Forgetting {
    shelves: Vec<(PathBuf, fsutil::FileLock)>,
    owner: PathBuf,
}

/// Lock an instance's snapshots (its mods and config, and its own worlds') for deleting,
/// or fail if a job is using them. Take this before deleting the instance itself.
pub fn lock_instance(id: &str) -> anyhow::Result<Forgetting> {
    let mut shelves = world_shelves(Owner::Instance(id))?;
    shelves.push(instance_dir(id));
    let mut held = Vec::new();
    for shelf in shelves {
        let l = try_lock(&shelf).with_context(|| format!("its backups in {}", shelf.display()))?;
        held.push((shelf, l));
    }
    Ok(Forgetting { shelves: held, owner: owner_dir(Owner::Instance(id)) })
}

impl Forgetting {
    /// Delete the snapshots, then each lock file while still holding it (anyone waiting
    /// on it finds it gone and locks afresh).
    pub fn forget(self) -> anyhow::Result<()> {
        let mut failed = Vec::new();
        for (shelf, l) in self.shelves {
            match std::fs::remove_dir_all(&shelf) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => failed.push(format!("{}: {e}", shelf.display())),
                _ => {
                    let _ = std::fs::remove_file(lock_path(&shelf));
                }
            }
            drop(l);
        }
        let _ = std::fs::remove_dir(&self.owner);
        if !failed.is_empty() {
            bail!("couldn't delete {}", failed.join("; "));
        }
        Ok(())
    }
}

/// Delete an instance's snapshots, if no job is using them.
#[cfg(test)]
pub fn forget_instance(id: &str) -> anyhow::Result<()> {
    lock_instance(id)?.forget()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;

    #[test]
    fn snapshots_are_whole_independent_copies() {
        let root = paths::test_root().join(format!("snap-{}", uuid::Uuid::new_v4().simple()));
        let world = root.join("saves/New World");
        std::fs::create_dir_all(world.join("region")).unwrap();
        std::fs::write(world.join("level.dat"), "L").unwrap();
        std::fs::write(world.join("region/r.0.0.mca"), "R").unwrap();
        let owner = format!("set {}", uuid::Uuid::new_v4().simple());
        let owner = Owner::Set(&owner);

        // A copy an earlier run left half done is cleared away, never listed.
        let into = world_dir(owner, "New World");
        std::fs::create_dir_all(into.join(".old.partial")).unwrap();
        let a = snapshot_world(&world, owner, "first", false).unwrap();
        let b = snapshot_world(&world, owner, "second", false).unwrap();
        assert_ne!(a, b, "two snapshots in one second still get their own folders");
        assert!(!into.join(".old.partial").exists());
        let list = snapshots(owner, "New World");
        assert_eq!(list.iter().map(|s| s.info.reason.as_str()).collect::<Vec<_>>(), ["second", "first"]);
        assert_eq!(std::fs::read_to_string(a.join("region/r.0.0.mca")).unwrap(), "R");
        assert_eq!(list[0].info.source, world.canonicalize().unwrap());

        // Playing on doesn't reach the snapshot.
        std::fs::write(world.join("level.dat"), "changed").unwrap();
        assert_eq!(std::fs::read_to_string(a.join("level.dat")).unwrap(), "L");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(owner_dir(owner)).unwrap();
    }

    #[test]
    fn only_automatic_snapshots_are_pruned() {
        let root = paths::test_root().join(format!("prune-{}", uuid::Uuid::new_v4().simple()));
        let world = root.join("saves/W");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), "L").unwrap();
        let id = format!("prune {}", uuid::Uuid::new_v4().simple());
        let owner = Owner::Instance(&id);
        snapshot_world(&world, owner, "by hand", false).unwrap();
        for _ in 0..4 {
            assert!(after_play(&world, owner, "A", 2).is_empty());
        }
        let left = snapshots(owner, "W");
        assert_eq!(left.iter().filter(|s| s.info.auto).count(), 2);
        assert!(left.iter().any(|s| s.info.reason == "by hand"), "a snapshot taken by hand stays");
        // Zero turns automatic backups off.
        assert!(after_play(&world, owner, "A", 0).is_empty());
        assert_eq!(snapshots(owner, "W").len(), 3);
        std::fs::remove_dir_all(root).unwrap();
        forget_instance(&id).unwrap();
        assert!(!owner_dir(owner).exists());
    }

    #[test]
    fn restoring_a_world_keeps_the_current_one_and_brings_back_deleted_ones() {
        let root = paths::test_root().join(format!("restore-{}", uuid::Uuid::new_v4().simple()));
        let saves = root.join("saves");
        let world = saves.join("W");
        std::fs::create_dir_all(world.join("region")).unwrap();
        std::fs::write(world.join("level.dat"), "old").unwrap();
        std::fs::write(world.join("region/r.0.0.mca"), "old").unwrap();
        let id = format!("restore {}", uuid::Uuid::new_v4().simple());
        let owner = Owner::Instance(&id);
        snapshot_world(&world, owner, "then", false).unwrap();

        // Played on: a file changes and one appears. Restoring brings back exactly the snapshot.
        std::fs::write(world.join("level.dat"), "new").unwrap();
        std::fs::write(world.join("region/r.1.0.mca"), "new").unwrap();
        let then = snapshots(owner, "W").into_iter().find(|s| s.info.reason == "then").unwrap();
        restore_world(&then, &saves).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("level.dat")).unwrap(), "old");
        assert!(!world.join("region/r.1.0.mca").exists());
        assert!(!world_dir(owner, "W").join(".restoring").exists());
        // What was there is a snapshot of its own, so the restore can be undone.
        let list = snapshots(owner, "W");
        assert_eq!(list.len(), 2);
        assert!(list[0].info.reason.starts_with("before restoring"));
        assert_eq!(std::fs::read_to_string(list[0].dir.join("level.dat")).unwrap(), "new");

        // A deleted world is still listed by its backups, and comes back from them.
        std::fs::remove_dir_all(&world).unwrap();
        assert_eq!(all_worlds(owner).get("W").map(Vec::len), Some(2));
        restore_world(&then, &saves).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("region/r.0.0.mca")).unwrap(), "old");
        assert_eq!(snapshots(owner, "W").len(), 2, "nothing to back up when the world was gone");

        // Deleting every snapshot of a world drops it from the list.
        for s in snapshots(owner, "W") {
            delete(&s).unwrap();
        }
        assert!(all_worlds(owner).is_empty());
        std::fs::remove_dir_all(root).unwrap();
        forget_instance(&id).unwrap();
    }

    #[test]
    fn jobs_on_one_shelf_wait_for_each_other() {
        let root = paths::test_root().join(format!("shelf-{}", uuid::Uuid::new_v4().simple()));
        let world = root.join("saves/W");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), "L").unwrap();
        let set = format!("set {}", uuid::Uuid::new_v4().simple());
        let owner = Owner::Set(&set);
        snapshot_world(&world, owner, "first", false).unwrap();
        let shelf = world_dir(owner, "W");

        // Another member's job is filling a copy on the shelf.
        let held = lock(&shelf).unwrap();
        let live = shelf.join(".2026-01-01T00-00-00.live.partial");
        std::fs::create_dir_all(&live).unwrap();
        let w = world.clone();
        let set2 = set.clone();
        let second = std::thread::spawn(move || snapshot_world(&w, Owner::Set(&set2), "second", false));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!second.is_finished(), "a second snapshot didn't wait for the shelf");
        assert!(live.is_dir(), "the other job's copy was cleared away under it");
        // Deleting from the UI doesn't wait; it says to try again.
        let first = snapshots(owner, "W").remove(0);
        assert!(delete(&first).is_err());
        drop(held);
        second.join().unwrap().unwrap();
        assert_eq!(snapshots(owner, "W").iter().map(|s| s.info.reason.as_str()).collect::<Vec<_>>(), ["second", "first"]);

        // Another process holding it counts the same.
        let holder = crate::fsutil::tests::FlockHolder::lock(&lock_path(&shelf));
        assert!(delete(&first).is_err());
        drop(holder);
        delete(&first).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(owner_dir(owner)).unwrap();
    }

    #[test]
    fn an_interrupted_restore_is_finished_before_launch() {
        use crate::fsutil::tests::{NO_EXCHANGE, write_journal};
        NO_EXCHANGE.with(|n| n.set(true));
        let mut inst = Instance::new("recover", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        let world = inst.saves_dir().join("W");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), "then").unwrap();
        let owner = Owner::of(&inst);
        snapshot_world(&world, owner, "then", false).unwrap();
        std::fs::write(world.join("level.dat"), "now").unwrap();

        // Without an atomic swap, a whole restore still works.
        let then = snapshots(owner, "W").remove(0);
        restore_world(&then, &inst.saves_dir()).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("level.dat")).unwrap(), "then");

        // Cut short after the world was moved aside, before the copy went in: no W in saves.
        let shelf = world_dir(owner, "W");
        let staged = shelf.join(".restoring");
        stage(&then.dir, &staged).unwrap();
        let target = inst.saves_dir().canonicalize().unwrap().join("W");
        write_journal(&staged, &target);
        std::fs::rename(&world, shelf.join(".restoring.old")).unwrap();
        assert!(!world.exists());
        // A snapshot taken on the shelf meanwhile leaves the finished copy alone.
        let elsewhere = inst.dir().join("elsewhere/W");
        std::fs::create_dir_all(&elsewhere).unwrap();
        snapshot_world(&elsewhere, owner, "x", false).unwrap();
        assert!(staged.is_dir());

        // The same for a mod rollback, with the mods folder moved aside.
        let mods = inst.game_dir().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("a.jar"), "new").unwrap();
        let ishelf = instance_dir(&inst.id);
        let mstaged = ishelf.join(".mods.restoring");
        std::fs::create_dir_all(&mstaged).unwrap();
        std::fs::write(mstaged.join("a.jar"), "old").unwrap();
        write_journal(&mstaged, &mods);
        std::fs::rename(&mods, ishelf.join(".mods.restoring.old")).unwrap();
        // And a copy cut short before its journal: never finished, so cleared away.
        std::fs::create_dir_all(ishelf.join(".config.restoring")).unwrap();

        recover(&inst).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("level.dat")).unwrap(), "then");
        assert_eq!(std::fs::read_to_string(mods.join("a.jar")).unwrap(), "old");
        for dir in [&shelf, &ishelf] {
            let left: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with('.')).collect();
            assert!(left.is_empty(), "{left:?} left in {}", dir.display());
        }
        NO_EXCHANGE.with(|n| n.set(false));
        std::fs::remove_dir_all(inst.dir()).unwrap();
        forget_instance(&inst.id).unwrap();
    }

    #[test]
    fn worlds_are_covered_once_until_played_again() {
        let mut inst = Instance::new("cover", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        let w = inst.saves_dir().join("W");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join("level.dat"), "L").unwrap();
        cover_worlds(&inst, "before updating").unwrap();
        cover_worlds(&inst, "before updating").unwrap();
        assert_eq!(snapshots(Owner::of(&inst), "W").len(), 1, "a snapshot since its last change covers it");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(w.join("level.dat"), "played").unwrap();
        cover_worlds(&inst, "before updating").unwrap();
        assert_eq!(snapshots(Owner::of(&inst), "W").len(), 2);
        std::fs::remove_dir_all(inst.dir()).unwrap();
        forget_instance(&inst.id).unwrap();
    }

    #[test]
    fn rolling_back_puts_mods_and_config_back() {
        let mut inst = Instance::new("rollback", "1.21.1", Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        let game = inst.game_dir();
        std::fs::create_dir_all(game.join("mods")).unwrap();
        std::fs::create_dir_all(game.join("config")).unwrap();
        std::fs::write(game.join("mods/a-1.0.jar"), "a1").unwrap();
        std::fs::write(game.join("mods/b.jar.disabled"), "b").unwrap();
        std::fs::write(game.join("config/a.toml"), "x = 1").unwrap();
        snapshot_instance(&inst, "before updating", 5).unwrap();

        // The update: a new jar replaces the old one, its config migrates, a dependency arrives.
        std::fs::remove_file(game.join("mods/a-1.0.jar")).unwrap();
        std::fs::write(game.join("mods/a-2.0.jar"), "a2").unwrap();
        std::fs::write(game.join("mods/dep.jar"), "d").unwrap();
        std::fs::write(game.join("config/a.toml"), "x = 2").unwrap();

        let snap = instance_snapshots(&inst.id).remove(0);
        roll_back(&inst, &snap, 5).unwrap();
        let mut mods: Vec<String> = std::fs::read_dir(game.join("mods")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        mods.sort();
        assert_eq!(mods, ["a-1.0.jar", "b.jar.disabled"]);
        assert_eq!(std::fs::read_to_string(game.join("config/a.toml")).unwrap(), "x = 1");
        // And the rollback can itself be undone.
        let list = instance_snapshots(&inst.id);
        assert_eq!(list.len(), 2);
        assert!(list[0].dir.join("mods/a-2.0.jar").exists());

        // Old ones are pruned, but never the one just taken.
        snapshot_instance(&inst, "again", 1).unwrap();
        assert_eq!(instance_snapshots(&inst.id).len(), 1);
        std::fs::remove_dir_all(inst.dir()).unwrap();
        forget_instance(&inst.id).unwrap();
    }

    #[test]
    fn launch_recovery_waits_for_a_busy_shelf_and_fails_loudly() {
        use crate::fsutil::tests::{NO_EXCHANGE, write_journal};
        NO_EXCHANGE.with(|n| n.set(true));
        let mut inst = Instance::new("recover busy", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        let world = inst.saves_dir().join("W");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), "then").unwrap();
        let owner = Owner::of(&inst);
        snapshot_world(&world, owner, "then", false).unwrap();
        let then = snapshots(owner, "W").remove(0);
        // A restore cut short with W moved aside, its shelf held by a job elsewhere.
        let shelf = world_dir(owner, "W");
        let staged = shelf.join(".restoring");
        stage(&then.dir, &staged).unwrap();
        write_journal(&staged, &inst.saves_dir().canonicalize().unwrap().join("W"));
        std::fs::rename(&world, shelf.join(".restoring.old")).unwrap();
        let held = lock(&shelf).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            drop(held);
        });
        // Waits past the UI's 200 ms rather than calling it done.
        recover(&inst).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("level.dat")).unwrap(), "then");
        releaser.join().unwrap();
        NO_EXCHANGE.with(|n| n.set(false));

        // A shelf that can't be put right stops the launch.
        if unsafe { libc::geteuid() } != 0 {
            let bad = shelf.join(".restoring");
            std::fs::create_dir_all(bad.join("inner")).unwrap();
            std::fs::set_permissions(&bad, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();
            assert!(recover(&inst).is_err());
            std::fs::set_permissions(&bad, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        }
        std::fs::remove_dir_all(inst.dir()).unwrap();
        forget_instance(&inst.id).unwrap();
    }

    #[test]
    fn an_unreadable_saves_folder_fails_the_update_backups() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let mut inst = Instance::new("cover unreadable", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        // No saves at all is fine: nothing to cover.
        cover_worlds(&inst, "x").unwrap();
        let w = inst.saves_dir().join("W");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join("level.dat"), "L").unwrap();
        for (dir, why) in [(inst.saves_dir(), "saves"), (w.clone(), "a world")] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            let r = cover_worlds(&inst, "x");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(r.is_err(), "an unreadable {why} looked like no worlds");
        }
        assert!(snapshots(Owner::of(&inst), "W").is_empty());
        cover_worlds(&inst, "x").unwrap();
        assert_eq!(snapshots(Owner::of(&inst), "W").len(), 1);
        std::fs::remove_dir_all(inst.dir()).unwrap();
        forget_instance(&inst.id).unwrap();
    }

    #[test]
    fn deleting_an_instance_waits_for_its_backup_jobs() {
        let mut inst = Instance::new("forget busy", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        let world = inst.saves_dir().join("W");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), "L").unwrap();
        snapshot_world(&world, Owner::of(&inst), "x", false).unwrap();
        let shelf = world_dir(Owner::of(&inst), "W");

        // A job (anywhere) holds the shelf: deleting refuses and leaves it all alone.
        let held = lock(&shelf).unwrap();
        assert!(lock_instance(&inst.id).is_err());
        assert_eq!(snapshots(Owner::of(&inst), "W").len(), 1);
        drop(held);

        // Once free, everything goes, lock files too, and a job waiting on a lock file
        // being deleted can't share the shelf with one that came after.
        let forgetting = lock_instance(&inst.id).unwrap();
        let s = shelf.clone();
        let waiter = std::thread::spawn(move || lock(&s).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(50));
        forgetting.forget().unwrap();
        assert!(!shelf.exists());
        let waited = waiter.join().unwrap();
        assert!(try_lock(&shelf).is_err(), "two holders of one shelf");
        drop(waited);
        let _ = std::fs::remove_file(lock_path(&shelf));
        let _ = std::fs::remove_dir(owner_dir(Owner::of(&inst)));
        assert!(!instance_dir(&inst.id).exists());
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }
}
