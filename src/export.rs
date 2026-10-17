//! Exporting instances: an instance zip in the MultiMC format that Prism and
//! PolyMC share (`instance.cfg`, `mmc-pack.json`
//! and `minecraft/`), or a Modrinth pack (`.mrpack`), where files Modrinth
//! hosts become downloads and everything else goes into `overrides/`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::json;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::content_meta::{self, Hosted};
use crate::instance::Instance;
use crate::loaders::Loader;
use crate::task::Task;
use crate::{fsutil, icons, net};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    Mrpack,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Mrpack => "mrpack",
        }
    }

    pub fn from_path(p: &Path) -> Option<Self> {
        match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "zip" => Some(Format::Zip),
            "mrpack" => Some(Format::Mrpack),
            _ => None,
        }
    }
}

/// Top-level entries of the game dir that are off by default: the player's
/// own worlds and pictures, logs, caches and per-machine state.
const PERSONAL: &[&str] = &[
    "saves",
    "screenshots",
    "logs",
    "crash-reports",
    "downloads",
    "server-resource-packs",
    "debug",
    "command_history.txt",
    "usercache.json",
    "usernamecache.json",
    "servers.dat_old",
    "debug-profile.json",
];

/// Something in the game dir that can go into the export.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Included unless the user says otherwise.
    pub default: bool,
}

/// The game dir's top-level entries, folders first. Measures sizes, so run it off the UI thread.
pub fn entries(inst: &Instance) -> Vec<Entry> {
    let mut out: Vec<Entry> = std::fs::read_dir(inst.game_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let mut ft = e.file_type().ok()?;
            // Shared folders export like the instance's own; other links stay out.
            if ft.is_symlink() {
                if !crate::shares::is_share_link(&e.path()) {
                    return None;
                }
                ft = std::fs::metadata(e.path()).ok()?.file_type();
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let size = if ft.is_dir() {
                fsutil::tree_size(&e.path())
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            };
            let default = !name.starts_with('.') && !PERSONAL.contains(&name.as_str());
            Some(Entry { name, is_dir: ft.is_dir(), size, default })
        })
        .collect();
    out.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
    out
}

pub fn default_include(inst: &Instance) -> Vec<String> {
    entries(inst).into_iter().filter(|e| e.default).map(|e| e.name).collect()
}

pub struct Request {
    pub inst: Instance,
    pub format: Format,
    pub dest: PathBuf,
    /// Top-level names in the game dir to include.
    pub include: Vec<String>,
    /// The pack's own version (mrpack only).
    pub version: String,
}

pub struct Exported {
    pub path: PathBuf,
    /// Files listed as Modrinth downloads rather than bundled (mrpack only).
    pub hosted: usize,
}

/// A file going into the export.
struct File {
    /// Relative to the game dir, with `/` separators.
    rel: String,
    path: PathBuf,
    size: u64,
    /// An empty folder, kept so the export matches the instance.
    dir: bool,
}

/// Write the export next to `dest` and move it into place, so a failed
/// export never leaves a half-written file behind.
pub async fn export(task: Task, req: Request) -> anyhow::Result<Exported> {
    if req.include.iter().any(|n| n.contains('/') || n == ".." || n.is_empty()) {
        bail!("bad entry in the include list");
    }
    let game = req.inst.game_dir();
    let include = req.include.clone();
    let t = task.clone();
    let files = tokio::task::spawn_blocking(move || {
        t.stage("Listing files", 0);
        collect(&game, &include)
    })
    .await??;

    let hosted = match req.format {
        Format::Zip => HashMap::new(),
        Format::Mrpack => find_hosted(&task, &files).await?,
    };

    let tmp = req.dest.with_file_name(format!(".{}.part", req.dest.file_name().context("no file name")?.to_string_lossy()));
    let (inst, format, version, t, dest_tmp) = (req.inst.clone(), req.format, req.version.clone(), task.clone(), tmp.clone());
    let n_hosted = hosted.len();
    let r = tokio::task::spawn_blocking(move || {
        let out = std::fs::File::create(&dest_tmp).with_context(|| format!("creating {}", dest_tmp.display()))?;
        let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(out));
        match format {
            Format::Zip => write_zip(&mut zip, &inst, &files, &t)?,
            Format::Mrpack => write_mrpack(&mut zip, &inst, &version, &files, &hosted, &t)?,
        }
        zip.finish()?.flush()?;
        anyhow::Ok(())
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|r| r);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &req.dest).with_context(|| format!("moving the export to {}", req.dest.display()))?;
    task.finish(&Ok(()));
    Ok(Exported { path: req.dest, hosted: n_hosted })
}

/// Every regular file and empty folder under the included entries. Symlinks are
/// skipped, except a top-level link to a shared folder, which is followed.
fn collect(game: &Path, include: &[String]) -> anyhow::Result<Vec<File>> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = include.iter().map(|n| (game.join(n), n.clone())).collect();
    while let Some((path, rel)) = stack.pop() {
        let top = !rel.contains('/') && crate::shares::is_share_link(&path);
        let Ok(meta) = (if top { std::fs::metadata(&path) } else { std::fs::symlink_metadata(&path) }) else { continue };
        if meta.is_dir() {
            let mut empty = true;
            for e in std::fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))? {
                let e = e?;
                stack.push((e.path(), format!("{rel}/{}", e.file_name().to_string_lossy())));
                empty = false;
            }
            if empty {
                out.push(File { rel, path, size: 0, dir: true });
            }
        } else if meta.is_file() {
            out.push(File { rel, path, size: meta.len(), dir: false });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// A bundled file's place on Modrinth: its sha1 and where it's hosted.
type Listed = HashMap<String, (String, Hosted)>;

/// Mods and packs that Modrinth hosts byte-for-byte, by path.
async fn find_hosted(task: &Task, files: &[File]) -> anyhow::Result<Listed> {
    // Disabled files stay in overrides, so they arrive disabled.
    let candidates: Vec<(String, PathBuf)> = files
        .iter()
        .filter(|f| !f.dir)
        .filter(|f| {
            let (dir, name) = f.rel.split_once('/').unwrap_or_default();
            let ext = if dir == "mods" { ".jar" } else { ".zip" };
            ["mods", "resourcepacks", "shaderpacks"].contains(&dir) && !name.contains('/') && name.ends_with(ext)
        })
        .map(|f| (f.rel.clone(), f.path.clone()))
        .collect();
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    let t = task.clone();
    let hashes: Vec<(String, String)> = tokio::task::spawn_blocking(move || {
        t.stage("Hashing mods and packs", candidates.len() as u64);
        candidates
            .into_iter()
            .filter_map(|(rel, path)| {
                t.inc(1);
                Some((rel, net::sha1_file(&path)?))
            })
            .collect()
    })
    .await?;
    task.stage("Looking up files on Modrinth", 0);
    let found = content_meta::hosted(hashes.iter().map(|(_, h)| h.clone()).collect())
        .await
        .context("looking up files on Modrinth (exporting an instance zip works offline)")?;
    Ok(hashes.into_iter().filter_map(|(rel, h)| Some((rel, (h.clone(), found.get(&h)?.clone())))).collect())
}

// -- writing --------------------------------------------------------------------------

type Zip = zip::ZipWriter<std::io::BufWriter<std::fs::File>>;

fn options(name: &str, size: u64) -> SimpleFileOptions {
    // Already-compressed formats only get bigger and slower with deflate.
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let stored = ["jar", "zip", "png", "jpg", "jpeg", "ogg", "mca", "gz", "dat", "mrpack"].contains(&ext.as_str());
    SimpleFileOptions::default()
        .compression_method(if stored { CompressionMethod::Stored } else { CompressionMethod::Deflated })
        .large_file(size >= u32::MAX as u64)
}

fn add_bytes(zip: &mut Zip, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    zip.start_file(name, options(name, bytes.len() as u64))?;
    zip.write_all(bytes)?;
    Ok(())
}

fn add_files<'a>(zip: &mut Zip, prefix: &str, files: impl Iterator<Item = &'a File>, task: &Task) -> anyhow::Result<()> {
    for f in files {
        let name = format!("{prefix}{}", f.rel);
        if f.dir {
            zip.add_directory(&name, SimpleFileOptions::default())?;
            continue;
        }
        zip.start_file(&name, options(&name, f.size))?;
        let mut src = std::fs::File::open(&f.path).with_context(|| format!("reading {}", f.path.display()))?;
        std::io::copy(&mut src, zip).with_context(|| format!("adding {}", f.rel))?;
        task.inc(f.size);
    }
    Ok(())
}

fn write_zip(zip: &mut Zip, inst: &Instance, files: &[File], task: &Task) -> anyhow::Result<()> {
    task.stage(format!("Writing {}", inst.name), files.iter().map(|f| f.size).sum::<u64>().max(1));
    let (icon_key, icon_file) = prism_icon(&inst.icon);
    add_bytes(zip, "instance.cfg", instance_cfg(inst, &icon_key).as_bytes())?;
    add_bytes(zip, "mmc-pack.json", serde_json::to_string_pretty(&mmc_pack(inst))?.as_bytes())?;
    if let Some((name, bytes)) = icon_file {
        add_bytes(zip, &name, &bytes)?;
    }
    add_files(zip, "minecraft/", files.iter(), task)
}

/// Prism's icon key, plus the image to ship beside instance.cfg if Prism
/// won't have it. Prism's built-in keys match ours, apart from our logo.
fn prism_icon(icon: &str) -> (String, Option<(String, Vec<u8>)>) {
    if let Some(file) = icon.strip_prefix("custom:") {
        let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).to_string();
        return match std::fs::read(icons::dir().join(file)) {
            Ok(bytes) => (stem, Some((file.to_string(), bytes))),
            Err(_) => ("default".into(), None),
        };
    }
    if icon == icons::DEFAULT || !icons::is_builtin(icon) {
        let bytes = icons::BUILTIN.iter().find(|(n, _)| *n == icons::DEFAULT).map(|(_, b)| b.to_vec()).unwrap_or_default();
        return (icons::DEFAULT.into(), Some((format!("{}.svg", icons::DEFAULT), bytes)));
    }
    (icon.to_string(), None)
}

/// A QSettings value, quoted and escaped when it needs to be.
fn cfg_value(v: &str) -> String {
    let plain = !v.is_empty()
        && v.trim() == v
        && !v.chars().any(|c| matches!(c, '"' | '\\' | '\n' | '\r' | '\t' | ',' | ';' | '=' | '#'));
    if plain || v.is_empty() {
        return v.to_string();
    }
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn instance_cfg(inst: &Instance, icon_key: &str) -> String {
    let mut kv: Vec<(&str, String)> = vec![
        ("ConfigVersion", "1.2".into()),
        ("InstanceType", "OneSix".into()),
        ("name", inst.name.clone()),
        ("iconKey", icon_key.to_string()),
        ("notes", inst.notes.clone()),
        ("totalTimePlayed", inst.played_secs.to_string()),
        ("lastLaunchTime", inst.last_played.map(|t| t.timestamp_millis()).unwrap_or(0).to_string()),
    ];
    if let Some(mb) = inst.memory_mb {
        kv.extend([("OverrideMemory", "true".into()), ("MaxMemAlloc", mb.to_string())]);
    }
    if !inst.jvm_args.is_empty() {
        kv.extend([("OverrideJavaArgs", "true".into()), ("JvmArgs", inst.jvm_args.clone())]);
    }
    if let Some((w, h)) = inst.window {
        kv.extend([("OverrideWindow", "true".into()), ("MinecraftWinWidth", w.to_string()), ("MinecraftWinHeight", h.to_string())]);
    }
    // The Java path is left out: it belongs to this machine.
    let mut out = String::from("[General]\n");
    for (k, v) in kv {
        out.push_str(&format!("{k}={}\n", cfg_value(&v)));
    }
    out
}

/// Prism fills in the rest (LWJGL and friends) when it loads the instance.
fn mmc_pack(inst: &Instance) -> serde_json::Value {
    let mut components = vec![json!({ "uid": "net.minecraft", "version": inst.mc_version, "important": true })];
    if let (Some(uid), Some(v)) = (inst.loader.kind().prism_uid(), inst.loader.version()) {
        if matches!(inst.loader, Loader::Fabric(_) | Loader::Quilt(_)) {
            components.push(json!({ "uid": "net.fabricmc.intermediary", "version": inst.mc_version, "dependencyOnly": true }));
        }
        components.push(json!({ "uid": uid, "version": v }));
    }
    json!({ "components": components, "formatVersion": 1 })
}

fn mrpack_index(inst: &Instance, version: &str, files: &[File], hosted: &Listed) -> serde_json::Value {
    let mut deps = serde_json::Map::new();
    deps.insert("minecraft".into(), json!(inst.mc_version));
    let key = match &inst.loader {
        Loader::Fabric(_) => Some("fabric-loader"),
        Loader::Quilt(_) => Some("quilt-loader"),
        l => l.modrinth(),
    };
    if let (Some(key), Some(v)) = (key, inst.loader.version()) {
        deps.insert(key.into(), json!(v));
    }
    let mut listed = Vec::new();
    for f in files {
        let Some((sha1, h)) = hosted.get(&f.rel) else { continue };
        listed.push(json!({
            "path": f.rel,
            "hashes": { "sha1": sha1, "sha512": h.sha512 },
            "downloads": [h.url],
            "fileSize": h.size,
        }));
    }
    let mut index = json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": if version.trim().is_empty() { "1.0.0" } else { version.trim() },
        "name": inst.name,
        "files": listed,
        "dependencies": deps,
    });
    if !inst.notes.is_empty() {
        index["summary"] = json!(inst.notes);
    }
    index
}

fn write_mrpack(zip: &mut Zip, inst: &Instance, version: &str, files: &[File], hosted: &Listed, task: &Task) -> anyhow::Result<()> {
    let bundled = || files.iter().filter(|f| !hosted.contains_key(&f.rel));
    task.stage(format!("Writing {}", inst.name), bundled().map(|f| f.size).sum::<u64>().max(1));
    let index = mrpack_index(inst, version, files, hosted);
    add_bytes(zip, "modrinth.index.json", serde_json::to_string_pretty(&index)?.as_bytes())?;
    add_files(zip, "overrides/", bundled(), task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> Instance {
        let mut i = Instance::new("My Pack", "1.21.1", Loader::Fabric("0.16.5".into()));
        i.notes = "line one\nline \"two\"".into();
        i.memory_mb = Some(6144);
        i.played_secs = 3600;
        i.window = Some((1280, 720));
        i
    }

    #[test]
    fn cfg_values_round_trip() {
        for v in ["plain", "with spaces", "a=b", "x\\y", "line one\nline \"two\"", " padded ", ""] {
            let line = format!("k={}\n", cfg_value(v));
            assert_eq!(crate::import::parse_cfg(&line)["k"], v, "{line:?}");
        }
    }

    #[test]
    fn prism_metadata_reads_back() {
        let i = inst();
        let cfg = instance_cfg(&i, "grass");
        let pack = mmc_pack(&i).to_string();
        let back = crate::import::prism_instance(&cfg, Some(&pack)).unwrap();
        assert_eq!(back.name, i.name);
        assert_eq!(back.mc_version, i.mc_version);
        assert_eq!(back.loader, i.loader);
        assert_eq!(back.notes, i.notes);
        assert_eq!(back.memory_mb, i.memory_mb);
        assert_eq!(back.played_secs, i.played_secs);
        assert_eq!(back.window, i.window);
    }

    #[test]
    fn icons_map_to_prism_keys() {
        assert_eq!(prism_icon("creeper"), ("creeper".into(), None));
        let (key, file) = prism_icon(icons::DEFAULT);
        assert_eq!(key, "omacraft");
        assert_eq!(file.unwrap().0, "omacraft.svg");
    }

    #[test]
    fn mrpack_lists_hosted_and_bundles_the_rest() {
        let dir = std::env::temp_dir().join(format!("omacraft-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        std::fs::write(dir.join("mods/a.jar"), b"a").unwrap();
        std::fs::write(dir.join("mods/b.jar"), b"b").unwrap();
        std::fs::write(dir.join("options.txt"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("config/empty")).unwrap();
        let files = collect(&dir, &["mods".into(), "options.txt".into(), "config".into()]).unwrap();
        assert_eq!(files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), ["config/empty", "mods/a.jar", "mods/b.jar", "options.txt"]);
        assert!(files[0].dir);
        let a = Hosted { url: "https://cdn.modrinth.com/a.jar".into(), sha512: "ff".into(), size: 1 };
        let hosted = HashMap::from([("mods/a.jar".to_string(), ("a1".to_string(), a))]);
        let index = mrpack_index(&inst(), "", &files, &hosted);
        assert_eq!(index["versionId"], "1.0.0");
        assert_eq!(index["dependencies"]["fabric-loader"], "0.16.5");
        assert_eq!(index["files"].as_array().unwrap().len(), 1);
        assert_eq!(index["files"][0]["path"], "mods/a.jar");
        assert_eq!(index["files"][0]["hashes"]["sha1"], "a1");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every file (with its bytes) and empty folder (`None`) under `dir`, by relative path.
    fn tree(dir: &Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let mut empty = true;
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                empty = false;
                let p = e.path();
                if std::fs::metadata(&p).unwrap().is_dir() {
                    stack.push(p);
                } else {
                    out.insert(p.strip_prefix(dir).unwrap().display().to_string(), Some(std::fs::read(&p).unwrap()));
                }
            }
            if empty && d != dir {
                out.insert(d.strip_prefix(dir).unwrap().display().to_string(), None);
            }
        }
        out
    }

    /// A saved instance with a custom icon, a few files of each kind, and a world
    /// (personal, so left out of exports by default). Nothing Modrinth could host,
    /// so the mrpack export stays offline.
    fn tiny_instance(scratch: &Path) -> Instance {
        let mut i = inst();
        i.jvm_args = r#"-Xss2m -Dgreeting="hi there""#.into();
        i.java_path = "/opt/java/bin/java".into();
        i.last_played = chrono::DateTime::from_timestamp_millis(1_700_000_000_123);
        std::fs::write(scratch.join("pic.png"), b"\x89PNG fake icon").unwrap();
        i.icon = icons::add_custom(&scratch.join("pic.png")).unwrap();
        i.assign_id().unwrap();
        i.save().unwrap();
        let g = i.game_dir();
        for (rel, bytes) in [
            ("options.txt", &b"fov:0.5\nlang:en_us\n"[..]),
            ("config/sodium.json", "{\"name\": \"caf\u{e9} \u{2603}\"}".as_bytes()),
            ("mods/old.jar.disabled", &[0u8, 1, 2, 255, 254, 80, 75][..]),
            ("saves/World/level.dat", b"nbt"),
        ] {
            std::fs::create_dir_all(g.join(rel).parent().unwrap()).unwrap();
            std::fs::write(g.join(rel), bytes).unwrap();
        }
        std::fs::create_dir_all(g.join("config/empty")).unwrap();
        i
    }

    async fn round_trip(src: &Instance, format: Format, scratch: &Path) -> (Instance, Exported) {
        let dest = scratch.join(format!("out.{}", format.ext()));
        let req = Request { inst: src.clone(), format, dest, include: default_include(src), version: "2.0".into() };
        let out = export(Task::new("export"), req).await.unwrap();
        let req = crate::import::Request { source: out.path.display().to_string(), name: None, group: String::new() };
        let back = crate::import::import(Task::new("import"), req).await.unwrap();
        assert!(back.warnings.is_empty(), "{:?}", back.warnings);
        (back.inst, out)
    }

    #[tokio::test]
    async fn exports_import_back() {
        let d = crate::paths::test_root().join(format!("roundtrip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let src = tiny_instance(&d);
        let mut want = tree(&src.game_dir());
        assert!(want.remove("saves/World/level.dat").is_some());
        assert_eq!(want.get("config/empty"), Some(&None));

        // Instance zip: files plus everything instance.cfg carries.
        let (back, _) = round_trip(&src, Format::Zip, &d).await;
        assert_ne!(back.id, src.id);
        assert_eq!(tree(&back.game_dir()), want);
        assert_eq!(
            (&back.name, &back.mc_version, &back.loader, &back.notes),
            (&src.name, &src.mc_version, &src.loader, &src.notes)
        );
        assert_eq!((back.memory_mb, &back.jvm_args, back.window), (src.memory_mb, &src.jvm_args, src.window));
        assert_eq!((back.played_secs, back.last_played), (src.played_secs, src.last_played));
        assert_eq!(back.java_path, "", "the Java path belongs to the exporting machine");
        let icon = |i: &Instance| std::fs::read(icons::dir().join(i.icon.strip_prefix("custom:").unwrap())).unwrap();
        assert_eq!(icon(&back), icon(&src));

        // Modrinth pack: files go through overrides; the index carries name, version, loader and notes.
        let (mr, out) = round_trip(&src, Format::Mrpack, &d).await;
        assert_eq!(out.hosted, 0);
        assert_eq!(tree(&mr.game_dir()), want);
        assert_eq!((&mr.name, &mr.mc_version, &mr.loader, &mr.notes), (&src.name, &src.mc_version, &src.loader, &src.notes));

        for i in [&src, &back, &mr] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn shared_folders_export_like_normal_ones() {
        let d = crate::paths::test_root().join(format!("shared-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let set = format!("export {tag}");
        let mut src = tiny_instance(&d);
        for (rel, bytes) in [("resourcepacks/pack.zip", &b"PK fake"[..]), ("schematics/house.litematic", b"h")] {
            let p = src.game_dir().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        // Another instance in the set, with packs of its own.
        let mut other = Instance::new("other", "1.21.1", Loader::Vanilla);
        other.share_set = set.clone();
        other.assign_id().unwrap();
        other.save().unwrap();
        for rel in ["resourcepacks/theirs.zip"] {
            std::fs::create_dir_all(other.game_dir().join(&rel).parent().unwrap()).unwrap();
            std::fs::write(other.game_dir().join(&rel), b"o").unwrap();
        }
        let mut want = tree(&src.game_dir());
        assert!(want.remove("saves/World/level.dat").is_some());
        want.insert("resourcepacks/theirs.zip".into(), Some(b"o".to_vec()));
        want.insert("shaderpacks".into(), None);
        src.share_set = set.clone();
        crate::shares::apply(&src).unwrap();
        crate::shares::apply(&other).unwrap();
        for f in ["resourcepacks", "schematics"] {
            assert!(crate::shares::is_share_link(&src.game_dir().join(f)), "{f}");
        }
        // A link that only looks like a share link stays out, even when asked for (G4).
        let outside = format!("outside-{tag}");
        std::fs::create_dir_all(crate::paths::data_dir().join(&outside)).unwrap();
        std::fs::write(crate::paths::data_dir().join(&outside).join("private.txt"), b"p").unwrap();
        std::os::unix::fs::symlink(format!("../../../shared/../{outside}"), src.game_dir().join("loot")).unwrap();
        assert!(!default_include(&src).contains(&"loot".to_string()));

        // The export holds real files: mods and config as always, and the set's packs.
        let dest = d.join("out.zip");
        let mut include = default_include(&src);
        include.push("loot".into());
        let req = Request { inst: src.clone(), format: Format::Zip, dest, include, version: String::new() };
        let out = export(Task::new("export"), req).await.unwrap();
        let req = crate::import::Request { source: out.path.display().to_string(), name: None, group: String::new() };
        let back = crate::import::import(Task::new("import"), req).await.unwrap().inst;
        assert_eq!(tree(&back.game_dir()), want);
        assert!(want.contains_key("mods/old.jar.disabled"));
        for e in std::fs::read_dir(back.game_dir()).unwrap().flatten() {
            assert!(!e.file_type().unwrap().is_symlink(), "{:?} is a link", e.file_name());
        }
        assert!(back.share_set.is_empty());

        for i in [&src, &other, &back] {
            std::fs::remove_dir_all(i.dir()).unwrap();
        }
        crate::shares::delete_set(&set).unwrap();
        std::fs::remove_dir_all(crate::paths::data_dir().join(&outside)).unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }
}
