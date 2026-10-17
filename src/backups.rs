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
//! swap in, so a crash never leaves a half-restored world or mods folder.

use std::collections::BTreeMap;
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

/// Take a snapshot into `shelf` (a folder of snapshots): `fill` copies into the
/// folder it's given. Returns the snapshot's folder.
fn take(shelf: &Path, source: &Path, reason: &str, auto: bool, fill: impl FnOnce(&Path) -> anyhow::Result<()>) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(shelf)?;
    // Copies cut short earlier are never finished: start over.
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
    let partial = shelf.join(format!(".{stamp}.partial"));
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
/// dot-folder the next snapshot there clears away.
pub fn delete(s: &Snapshot) -> anyhow::Result<()> {
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

/// Delete the oldest automatic snapshots in `shelf` past the newest `keep`.
fn prune(shelf: &Path, keep: usize) -> Vec<String> {
    list(shelf).iter().filter(|s| s.info.auto).skip(keep).filter_map(|s| delete(s).err().map(|e| format!("couldn't prune {}: {e:#}", s.dir.display()))).collect()
}

/// Snapshot the world at `world` (a folder in some `saves/`). Returns the snapshot's folder.
pub fn snapshot_world(world: &Path, owner: Owner, reason: &str, auto: bool) -> anyhow::Result<PathBuf> {
    let folder = world.file_name().context("world has no folder name")?.to_string_lossy().into_owned();
    take(&world_dir(owner, &folder), world, reason, auto, |into| {
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
    match snapshot_world(world, owner, &format!("after {inst_name} played it"), true) {
        Ok(_) => prune(&world_dir(owner, &name), keep as usize),
        Err(e) => vec![format!("couldn't back up {name}: {e:#}")],
    }
}

/// Snapshot every world in `inst`'s saves that no snapshot covers since it was last
/// changed, so a rollback has a version from before now to go back to. Skips worlds
/// open in a game. Returns warnings.
pub fn cover_worlds(inst: &Instance, reason: &str) -> Vec<String> {
    let owner = Owner::of(inst);
    let mut notes = Vec::new();
    for w in crate::saves::list_worlds(&inst.saves_dir()) {
        let modified = std::fs::metadata(w.dir.join("level.dat")).and_then(|m| m.modified()).ok().map(DateTime::<Utc>::from);
        let covered = snapshots(owner, &w.folder).iter().any(|s| modified.is_some_and(|m| s.info.taken >= m));
        if covered || crate::world_sets::lock_holder(&w.dir).is_some() {
            continue;
        }
        if let Err(e) = snapshot_world(&w.dir, owner, reason, false) {
            notes.push(format!("couldn't back up {}: {e:#}", w.name));
        }
    }
    notes
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
pub fn restore_world(s: &Snapshot, saves: &Path, owner: Owner) -> anyhow::Result<()> {
    let shelf = s.dir.parent().context("snapshot has no parent folder")?;
    let folder = shelf.file_name().context("snapshot has no world")?.to_string_lossy().into_owned();
    let world = saves.join(&folder);
    if crate::world_sets::lock_holder(&world).is_some() {
        bail!("“{folder}” is open in a game; close it first");
    }
    if world.join("level.dat").is_file() {
        snapshot_world(&world, owner, &format!("before restoring the backup from {}", s.when()), false).context("backing up the world as it is now")?;
    }
    // Staged beside the snapshots, not in saves/, where the game would list it.
    let staged = shelf.join(".restoring.partial");
    let _ = std::fs::remove_dir_all(&staged);
    if let Err(e) = fsutil::copy_tree(&s.dir, &staged, &|_| {}) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e).context("copying the backup");
    }
    std::fs::create_dir_all(saves)?;
    fsutil::swap_in(&staged, &world).with_context(|| format!("putting it in {}", world.display()))?;
    Ok(())
}

/// What an instance snapshot holds.
pub const INSTANCE_FOLDERS: [&str; 2] = ["mods", "config"];

/// Snapshot `inst`'s mods and config, then prune its snapshots to `keep` (at least one,
/// this one). Returns the snapshot's folder.
pub fn snapshot_instance(inst: &Instance, reason: &str, keep: u32) -> anyhow::Result<PathBuf> {
    let done = take_instance(inst, reason)?;
    prune_instance(&inst.id, keep);
    Ok(done)
}

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
    // Pruned only once done: `s` may be the oldest.
    take_instance(inst, &format!("before rolling back to {}", s.when())).context("backing up the mods as they are now")?;
    let game = inst.game_dir();
    for f in INSTANCE_FOLDERS {
        let from = s.dir.join(f);
        if !from.is_dir() {
            continue;
        }
        let staged = instance_dir(&inst.id).join(format!(".{f}.restoring.partial"));
        let _ = std::fs::remove_dir_all(&staged);
        if let Err(e) = fsutil::copy_tree(&from, &staged, &|_| {}) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e).with_context(|| format!("copying the backed up {f}"));
        }
        fsutil::swap_in(&staged, &game.join(f)).with_context(|| format!("putting {f} back"))?;
    }
    prune_instance(&inst.id, keep);
    Ok(())
}

/// Delete an instance's snapshots: its mods and config, and its own worlds'.
pub fn forget_instance(id: &str) {
    let _ = std::fs::remove_dir_all(instance_dir(id));
    let _ = std::fs::remove_dir_all(owner_dir(Owner::Instance(id)));
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
        forget_instance(&id);
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
        restore_world(&then, &saves, owner).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("level.dat")).unwrap(), "old");
        assert!(!world.join("region/r.1.0.mca").exists());
        assert!(!world_dir(owner, "W").join(".restoring.partial").exists());
        // What was there is a snapshot of its own, so the restore can be undone.
        let list = snapshots(owner, "W");
        assert_eq!(list.len(), 2);
        assert!(list[0].info.reason.starts_with("before restoring"));
        assert_eq!(std::fs::read_to_string(list[0].dir.join("level.dat")).unwrap(), "new");

        // A deleted world is still listed by its backups, and comes back from them.
        std::fs::remove_dir_all(&world).unwrap();
        assert_eq!(all_worlds(owner).get("W").map(Vec::len), Some(2));
        restore_world(&then, &saves, owner).unwrap();
        assert_eq!(std::fs::read_to_string(world.join("region/r.0.0.mca")).unwrap(), "old");
        assert_eq!(snapshots(owner, "W").len(), 2, "nothing to back up when the world was gone");

        // Deleting every snapshot of a world drops it from the list.
        for s in snapshots(owner, "W") {
            delete(&s).unwrap();
        }
        assert!(all_worlds(owner).is_empty());
        std::fs::remove_dir_all(root).unwrap();
        forget_instance(&id);
    }

    #[test]
    fn worlds_are_covered_once_until_played_again() {
        let mut inst = Instance::new("cover", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        let w = inst.saves_dir().join("W");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join("level.dat"), "L").unwrap();
        assert!(cover_worlds(&inst, "before updating").is_empty());
        assert!(cover_worlds(&inst, "before updating").is_empty());
        assert_eq!(snapshots(Owner::of(&inst), "W").len(), 1, "a snapshot since its last change covers it");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(w.join("level.dat"), "played").unwrap();
        cover_worlds(&inst, "before updating");
        assert_eq!(snapshots(Owner::of(&inst), "W").len(), 2);
        std::fs::remove_dir_all(inst.dir()).unwrap();
        forget_instance(&inst.id);
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
        forget_instance(&inst.id);
    }
}
