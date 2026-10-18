//! Worlds (`saves/*/level.dat`) and the multiplayer server list
//! (`servers.dat`), both NBT. Equivalent of Prism's WorldList / ServersModel.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, Utc};
use fastnbt::Value;
use serde::Deserialize;

use crate::fsutil;

#[derive(Debug, Clone)]
pub struct World {
    pub folder: String,
    pub dir: PathBuf,
    pub name: String,
    pub last_played: Option<DateTime<Utc>>,
    pub mode: &'static str,
    pub hardcore: bool,
    pub version: Option<String>,
    pub icon: Option<PathBuf>,
    /// (instance id, name) it was last played in, from Omacraft's record, if read.
    pub played_in: Option<(String, String)>,
}

#[derive(Deserialize)]
struct LevelDat {
    #[serde(rename = "Data")]
    data: LevelData,
}

#[derive(Deserialize)]
struct LevelData {
    #[serde(rename = "LevelName")]
    name: Option<String>,
    #[serde(rename = "LastPlayed")]
    last_played: Option<i64>,
    #[serde(rename = "GameType")]
    game_type: Option<i32>,
    hardcore: Option<i8>,
    #[serde(rename = "Version")]
    version: Option<LevelVersion>,
}

#[derive(Deserialize)]
struct LevelVersion {
    #[serde(rename = "Name")]
    name: Option<String>,
}

fn read_gz(path: &Path) -> anyhow::Result<Vec<u8>> {
    Ok(fsutil::read_capped(flate2::read::GzDecoder::new(std::fs::File::open(path)?))?)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    Ok(fsutil::write_atomic(path, bytes, 0o644)?)
}

pub fn list_worlds(saves: &Path) -> Vec<World> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(saves).into_iter().flatten().flatten() {
        let dir = e.path();
        let level = dir.join("level.dat");
        if !level.is_file() {
            continue;
        }
        let folder = e.file_name().to_string_lossy().into_owned();
        let data = read_gz(&level).ok().and_then(|b| fastnbt::from_bytes::<LevelDat>(&b).ok()).map(|l| l.data);
        let icon = Some(dir.join("icon.png")).filter(|p| p.is_file());
        let (name, last_played, mode, hardcore, version) = match data {
            Some(d) => (
                d.name.filter(|n| !n.is_empty()).unwrap_or_else(|| folder.clone()),
                d.last_played.and_then(DateTime::from_timestamp_millis),
                match d.game_type {
                    Some(1) => "creative",
                    Some(2) => "adventure",
                    Some(3) => "spectator",
                    _ => "survival",
                },
                d.hardcore.unwrap_or(0) != 0,
                d.version.and_then(|v| v.name),
            ),
            None => (folder.clone(), None, "unknown", false, None),
        };
        out.push(World { folder, dir, name, last_played, mode, hardcore, version, icon, played_in: None });
    }
    out.sort_by(|a, b| b.last_played.cmp(&a.last_played).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Change the name shown in the world list (not the folder).
pub fn rename_world(dir: &Path, name: &str) -> anyhow::Result<()> {
    let path = dir.join("level.dat");
    let mut root: Value = fastnbt::from_bytes(&read_gz(&path)?)?;
    let Value::Compound(top) = &mut root else { anyhow::bail!("level.dat isn't a compound") };
    let Some(Value::Compound(data)) = top.get_mut("Data") else { anyhow::bail!("level.dat has no Data") };
    data.insert("LevelName".into(), Value::String(name.to_string()));
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&fastnbt::to_bytes(&root)?)?;
    write_atomic(&path, &gz.finish()?)
}

/// Copy a world next to itself as "<name> (copy)". Returns the new folder.
pub fn duplicate_world(w: &World) -> anyhow::Result<String> {
    let parent = w.dir.parent().context("world has no parent")?;
    let mut folder = format!("{} (copy)", w.folder);
    let mut n = 2;
    while parent.join(&folder).exists() {
        folder = format!("{} (copy {n})", w.folder);
        n += 1;
    }
    let dst = parent.join(&folder);
    fsutil::copy_tree(&w.dir, &dst, &|_| {})?;
    let _ = std::fs::remove_file(dst.join("session.lock"));
    rename_world(&dst, &format!("{} (copy)", w.name))?;
    Ok(folder)
}

/// Add a world from outside the instance: a world folder (one with `level.dat`) or a
/// `.zip` holding one, at its root or in a folder. Copied (or extracted) beside `saves`,
/// never inside it where the game would list a half-made world, then moved in under the
/// next free name ("<name> (2)"…). A folder open in a game is refused. Returns the folder.
pub fn add_world(src: &Path, saves: &Path) -> anyhow::Result<String> {
    let file_name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let is_zip = src.is_file() && file_name.to_lowercase().ends_with(".zip");
    if !(src.join("level.dat").is_file() || is_zip) {
        anyhow::bail!("{file_name} isn't a world (a folder with level.dat, or a .zip of one)");
    }
    if !is_zip && crate::world_sets::lock_holder(src).is_some() {
        anyhow::bail!("{file_name} is open in a game; close it first");
    }
    std::fs::create_dir_all(saves)?;
    let saves = saves.canonicalize()?;
    let stage = saves.parent().context("saves has no parent")?.join(format!(".adding-world-{}", uuid::Uuid::new_v4()));
    let made = if is_zip { extract_world(src, &stage) } else { fsutil::copy_tree(src, &stage, &|_| {}).map_err(Into::into).map(|_| file_name.clone()) };
    let moved = made.and_then(|name| {
        let _ = std::fs::remove_file(stage.join("session.lock"));
        anyhow::ensure!(stage.join("level.dat").is_file(), "{file_name} has no level.dat");
        let mut folder = name.clone();
        for n in 2.. {
            match fsutil::rename_noreplace(&stage, &saves.join(&folder)) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(libc::ENOTEMPTY) => folder = format!("{name} ({n})"),
                r => return r.map(|_| folder).map_err(Into::into),
            }
        }
        unreachable!()
    });
    if moved.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    moved.with_context(|| format!("Couldn't add {file_name}"))
}

/// Extract the one world in `zip` into `dst`, returning its name: the folder holding
/// `level.dat`, or the zip's own name when that's at the root.
fn extract_world(zip_path: &Path, dst: &Path) -> anyhow::Result<String> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path)?)?;
    let roots: Vec<String> = zip.file_names().flatten().filter_map(|n| n.strip_suffix("level.dat").filter(|p| p.is_empty() || p.ends_with('/')).map(str::to_string)).collect();
    // Worlds' own folders can hold more level.dat files (old backups); only the outermost counts.
    let mut outer: Vec<&String> = roots.iter().filter(|r| !roots.iter().any(|o| o.len() < r.len() && r.starts_with(o.as_str()))).collect();
    outer.dedup();
    let prefix = match outer.as_slice() {
        [one] => one.to_string(),
        [] => anyhow::bail!("no world in it (no level.dat)"),
        _ => anyhow::bail!("it holds {} worlds; drop them one at a time", outer.len()),
    };
    if !prefix.is_empty() && crate::import::safe_rel(prefix.trim_end_matches('/')).is_none() {
        anyhow::bail!("its world folder has an unsafe path");
    }
    for i in 0..zip.len() {
        let mut e = zip.by_index(i)?;
        let name = e.name()?.to_string();
        let Some(rel) = name.strip_prefix(&prefix) else { continue };
        let Some(rel) = crate::import::safe_rel(rel.trim_end_matches('/')) else { continue };
        let out = dst.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        std::fs::create_dir_all(out.parent().unwrap())?;
        std::io::copy(&mut e, &mut std::fs::File::create(&out)?)?;
    }
    let name = prefix.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_string);
    Ok(name.unwrap_or_else(|| zip_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "World".into())))
}

// -- servers.dat ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub ip: String,
    /// PNG bytes from the server's last ping.
    pub icon: Option<Vec<u8>>,
    /// The original compound, so fields we don't edit survive a save.
    raw: Option<Value>,
}

impl Server {
    pub fn new(name: &str, ip: &str) -> Self {
        Server { name: name.into(), ip: ip.into(), icon: None, raw: None }
    }
}

fn servers_path(game_dir: &Path) -> PathBuf {
    game_dir.join("servers.dat")
}

pub fn read_servers(game_dir: &Path) -> anyhow::Result<Vec<Server>> {
    let path = servers_path(game_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let root: Value = fastnbt::from_bytes(&std::fs::read(&path)?).context("reading servers.dat")?;
    let Value::Compound(top) = root else { return Ok(Vec::new()) };
    let Some(Value::List(list)) = top.get("servers") else { return Ok(Vec::new()) };
    Ok(list
        .iter()
        .filter_map(|v| {
            let Value::Compound(c) = v else { return None };
            let s = |k: &str| match c.get(k) {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let icon = match c.get("icon") {
                Some(Value::String(b)) => base64::engine::general_purpose::STANDARD.decode(b).ok(),
                _ => None,
            };
            Some(Server { name: s("name"), ip: s("ip"), icon, raw: Some(v.clone()) })
        })
        .collect())
}

pub fn write_servers(game_dir: &Path, servers: &[Server]) -> anyhow::Result<()> {
    let list: Vec<Value> = servers
        .iter()
        .map(|s| {
            let mut c = match &s.raw {
                Some(Value::Compound(c)) => c.clone(),
                _ => Default::default(),
            };
            c.insert("name".into(), Value::String(s.name.clone()));
            c.insert("ip".into(), Value::String(s.ip.clone()));
            Value::Compound(c)
        })
        .collect();
    // Replace only the list: anything else at the root survives.
    let mut top = match std::fs::read(servers_path(game_dir)).ok().and_then(|b| fastnbt::from_bytes::<Value>(&b).ok()) {
        Some(Value::Compound(c)) => c,
        _ => Default::default(),
    };
    top.insert("servers".to_string(), Value::List(list));
    let root = Value::Compound(top);
    std::fs::create_dir_all(game_dir)?;
    write_atomic(&servers_path(game_dir), &fastnbt::to_bytes(&root)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn compound(fields: Vec<(&str, Value)>) -> Value {
        Value::Compound(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>())
    }

    /// A small `.minecraft` with one world and a server list, both carrying
    /// fields we don't model (nested, arrays, at the root), so round trips
    /// prove they survive.
    fn synthetic() -> PathBuf {
        let dir = crate::paths::test_root().join(format!("saves-{}", uuid::Uuid::new_v4()));
        let world = dir.join("saves/Wörld");
        std::fs::create_dir_all(&world).unwrap();
        let level = compound(vec![
            ("Data", compound(vec![
                ("LevelName", Value::String("Wörld ✓".into())),
                ("LastPlayed", Value::Long(1_789_824_260_016)),
                ("GameType", Value::Int(1)),
                ("hardcore", Value::Byte(0)),
                ("Version", compound(vec![("Name", Value::String("1.21.1".into())), ("Id", Value::Int(3955))])),
                ("DataPacks", compound(vec![("Enabled", Value::List(vec![Value::String("vanilla".into())]))])),
                ("WanderingTraderId", Value::IntArray(fastnbt::IntArray::new(vec![1, -2, 3, -4]))),
                ("Seeds", Value::LongArray(fastnbt::LongArray::new(vec![i64::MAX, -1]))),
            ])),
            ("extra_root", Value::Short(7)),
        ]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&fastnbt::to_bytes(&level).unwrap()).unwrap();
        std::fs::write(world.join("level.dat"), gz.finish().unwrap()).unwrap();
        std::fs::write(world.join("session.lock"), "☃").unwrap();

        let servers = compound(vec![
            ("servers", Value::List(vec![
                compound(vec![("name", Value::String("Ünïcode".into())), ("ip", Value::String("a.example:25570".into())), ("acceptTextures", Value::Byte(1))]),
                compound(vec![("name", Value::String("Two".into())), ("ip", Value::String("b.example".into())), ("icon", Value::String("iVBORw0KGgo=".into()))]),
            ])),
            ("extra_root_tag", Value::Int(7)),
        ]);
        std::fs::write(dir.join("servers.dat"), fastnbt::to_bytes(&servers).unwrap()).unwrap();
        dir
    }

    /// The synthetic fixture, plus a copy of real game data when
    /// OMACRAFT_FIXTURE points at a `.minecraft`-like folder.
    fn fixtures() -> Vec<PathBuf> {
        let mut out = vec![synthetic()];
        if let Ok(src) = std::env::var("OMACRAFT_FIXTURE") {
            let dst = crate::paths::test_root().join(format!("fixture-{}", uuid::Uuid::new_v4()));
            fsutil::copy_tree(Path::new(&src), &dst, &|_| {}).unwrap();
            out.push(dst);
        }
        out
    }

    #[test]
    fn worlds_and_rename_round_trip() {
        for dir in fixtures() {
            let worlds = list_worlds(&dir.join("saves"));
            assert!(!worlds.is_empty());
            let w = &worlds[0];
            let before: Value = fastnbt::from_bytes(&read_gz(&w.dir.join("level.dat")).unwrap()).unwrap();
            rename_world(&w.dir, "Renamed").unwrap();
            let mut after: Value = fastnbt::from_bytes(&read_gz(&w.dir.join("level.dat")).unwrap()).unwrap();
            // Only LevelName may differ.
            if let Value::Compound(top) = &mut after
                && let Some(Value::Compound(d)) = top.get_mut("Data")
            {
                assert_eq!(d.get("LevelName"), Some(&Value::String("Renamed".into())));
                if let Value::Compound(b) = &before
                    && let Some(Value::Compound(bd)) = b.get("Data")
                {
                    d.insert("LevelName".into(), bd["LevelName"].clone());
                }
            }
            assert_eq!(before, after);
            let copy = duplicate_world(&list_worlds(&dir.join("saves")).into_iter().find(|x| x.folder == w.folder).unwrap()).unwrap();
            assert!(list_worlds(&dir.join("saves")).iter().any(|x| x.folder == copy && x.name == "Renamed (copy)"));
            assert!(!dir.join("saves").join(&copy).join("session.lock").exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn dropped_worlds_are_added_beside_the_others() {
        let dir = synthetic();
        let (world, saves) = (dir.join("saves/Wörld"), dir.join("other/saves"));
        let staged = || std::fs::read_dir(dir.join("other")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with('.'));
        // A folder, twice: the second gets the next free name, and neither keeps the lock file.
        assert_eq!(add_world(&world, &saves).unwrap(), "Wörld");
        assert_eq!(add_world(&world, &saves).unwrap(), "Wörld (2)");
        assert!(!saves.join("Wörld/session.lock").exists());
        assert_eq!(list_worlds(&saves)[0].name, "Wörld ✓");

        let zip_of = |name: &str, entries: &[(&str, &[u8])]| {
            let p = dir.join(name);
            let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
            for (n, b) in entries {
                z.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(b).unwrap();
            }
            z.finish().unwrap();
            p
        };
        let level = std::fs::read(world.join("level.dat")).unwrap();
        // A world in a folder of the zip, with an old level.dat inside it and a path that climbs out.
        let z = zip_of("pack.zip", &[("Big Base/level.dat", &level), ("Big Base/backup/level.dat", b"old"), ("Big Base/region/r.0.0.mca", b"r"), ("Big Base/../../escape", b"x")]);
        assert_eq!(add_world(&z, &saves).unwrap(), "Big Base");
        assert!(saves.join("Big Base/region/r.0.0.mca").is_file() && !dir.join("escape").exists());
        // level.dat at the zip's root: named after the zip.
        let z = zip_of("Flat Land.zip", &[("level.dat", &level)]);
        assert_eq!(add_world(&z, &saves).unwrap(), "Flat Land");
        // Two worlds, none, or not a world at all: nothing is added.
        let before = list_worlds(&saves).len();
        assert!(add_world(&zip_of("two.zip", &[("A/level.dat", &level), ("B/level.dat", &level)]), &saves).is_err());
        assert!(add_world(&zip_of("none.zip", &[("readme.txt", b"hi")]), &saves).is_err());
        assert!(add_world(&dir.join("saves"), &saves).is_err());
        assert_eq!(list_worlds(&saves).len(), before);
        assert!(!staged(), "no staged copy is left behind");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn synthetic_world_reads() {
        let dir = synthetic();
        let w = &list_worlds(&dir.join("saves"))[0];
        assert_eq!((w.name.as_str(), w.mode, w.hardcore, w.version.as_deref()), ("Wörld ✓", "creative", false, Some("1.21.1")));
        assert!(w.last_played.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn servers_round_trip() {
        for dir in fixtures() {
            if !dir.join("servers.dat").exists() {
                continue;
            }
            let before: Value = fastnbt::from_bytes(&std::fs::read(dir.join("servers.dat")).unwrap()).unwrap();
            let servers = read_servers(&dir).unwrap();
            write_servers(&dir, &servers).unwrap();
            let after: Value = fastnbt::from_bytes(&std::fs::read(dir.join("servers.dat")).unwrap()).unwrap();
            assert_eq!(before, after, "unchanged list must write back identically");

            let mut edited = servers.clone();
            edited.push(Server::new("Test", "example.com:25566"));
            let last = edited.len() - 1;
            edited.swap(0, last);
            write_servers(&dir, &edited).unwrap();
            let again = read_servers(&dir).unwrap();
            assert_eq!(again[0].ip, "example.com:25566");
            assert_eq!(again.len(), servers.len() + 1);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
