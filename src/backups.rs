//! World snapshots: reflink copies under `backups/worlds/`, free on btrfs and xfs.
//! A world set's worlds go under `sets/<set>/<world folder>/`, an instance's own
//! under `instances/<id>/<world folder>/`. Each snapshot is a folder named for when
//! it was taken (UTC), next to a `<stamp>.toml` saying why. It's copied under a
//! dot-name and renamed in once whole, so a snapshot that's there is complete.
//!
//! This is the primitive the pre-launch world check uses; restoring and scheduled
//! backups come later.

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{fsutil, paths};

/// Whose world a snapshot is.
#[derive(Debug, Clone, Copy)]
pub enum Owner<'a> {
    Set(&'a str),
    Instance(&'a str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub taken: DateTime<Utc>,
    pub reason: String,
    /// The world folder it was copied from.
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub info: Info,
}

pub fn dir() -> PathBuf {
    paths::data_dir().join("backups").join("worlds")
}

/// Where snapshots of world folder `world` are kept.
pub fn world_dir(owner: Owner, world: &str) -> PathBuf {
    match owner {
        Owner::Set(s) => dir().join("sets").join(s).join(world),
        Owner::Instance(i) => dir().join("instances").join(i).join(world),
    }
}

/// Snapshot the world at `world` (a folder in some `saves/`). Returns the snapshot's folder.
pub fn snapshot_world(world: &Path, owner: Owner, reason: &str) -> anyhow::Result<PathBuf> {
    let folder = world.file_name().context("world has no folder name")?.to_string_lossy().into_owned();
    let into = world_dir(owner, &folder);
    std::fs::create_dir_all(&into)?;
    // Copies cut short earlier are never finished: start over.
    for e in std::fs::read_dir(&into)?.flatten() {
        if e.file_name().to_string_lossy().ends_with(".partial") {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let taken = Utc::now();
    let base = taken.format("%Y-%m-%dT%H-%M-%S").to_string();
    let stamp = (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|s| std::fs::symlink_metadata(into.join(s)).is_err() && std::fs::symlink_metadata(into.join(format!("{s}.toml"))).is_err())
        .unwrap();
    let partial = into.join(format!(".{stamp}.partial"));
    let made = fsutil::copy_tree(world, &partial, &|_| {});
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e).with_context(|| format!("copying {}", world.display()));
    }
    // The running game's lock, which means nothing in a copy.
    let _ = std::fs::remove_file(partial.join("session.lock"));
    // The real folder, not the path through an instance's share link.
    let source = world.canonicalize().unwrap_or_else(|_| world.to_path_buf());
    let info = Info { taken, reason: reason.to_string(), source };
    fsutil::write_atomic(&into.join(format!("{stamp}.toml")), toml::to_string_pretty(&info)?.as_bytes(), 0o644)?;
    let done = into.join(&stamp);
    std::fs::rename(&partial, &done)?;
    Ok(done)
}

/// The finished snapshots of world folder `world`, newest first.
pub fn snapshots(owner: Owner, world: &str) -> Vec<Snapshot> {
    let into = world_dir(owner, world);
    let mut out: Vec<Snapshot> = std::fs::read_dir(&into)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.') && e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let text = std::fs::read_to_string(into.join(format!("{}.toml", e.file_name().to_string_lossy()))).ok()?;
            Some(Snapshot { dir: e.path(), info: toml::from_str(&text).ok()? })
        })
        .collect();
    out.sort_by(|a, b| b.info.taken.cmp(&a.info.taken));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let a = snapshot_world(&world, owner, "first").unwrap();
        let b = snapshot_world(&world, owner, "second").unwrap();
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
        std::fs::remove_dir_all(into).unwrap();
    }
}
