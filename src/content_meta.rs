//! What an installed mod or pack is: its name, version and icon. Read from
//! the file itself where possible (fabric.mod.json, quilt.mod.json, Forge's
//! mods.toml or mcmod.info, pack.png), then topped up from Modrinth by hash,
//! which also covers shader packs. Icons are cached on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::instance::ContentKind;
use crate::net::{self, Download};
use crate::{fsutil, paths};

#[derive(Debug, Clone, Default)]
pub struct FileMeta {
    pub name: Option<String>,
    pub version: Option<String>,
    pub icon: Option<PathBuf>,
    pub project_id: Option<String>,
    pub sha1: Option<String>,
    /// The id the mod gives itself (`fabric.mod.json` `id`, Forge's `modId`).
    pub mod_id: Option<String>,
}

pub fn icon_dir() -> PathBuf {
    paths::data_dir().join("cache").join("icons")
}

fn ext_of(name: &str) -> &str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
        Some(e) if ["png", "jpg", "jpeg", "webp", "gif", "svg"].contains(&e.as_str()) => {
            match e.as_str() {
                "png" => "png",
                "jpg" | "jpeg" => "jpg",
                "webp" => "webp",
                "gif" => "gif",
                _ => "svg",
            }
        }
        _ => "png",
    }
}

fn save_icon(key: &str, ext: &str, bytes: &[u8]) -> Option<PathBuf> {
    let path = icon_dir().join(format!("{}.{ext}", net::short_hash(key.as_bytes())));
    if !path.exists() {
        std::fs::create_dir_all(icon_dir()).ok()?;
        std::fs::write(&path, bytes).ok()?;
    }
    Some(path)
}

fn zip_bytes(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    fsutil::read_capped(zip.by_name(name.trim_start_matches('/')).ok()?).ok()
}

/// Fabric's `icon` is a path or a map of size → path; take the largest ≤ 128.
fn fabric_icon(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => {
            let mut sizes: Vec<(u32, &str)> = m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()?))).collect();
            sizes.sort();
            sizes.iter().rev().find(|(s, _)| *s <= 128).or(sizes.first()).map(|(_, p)| p.to_string())
        }
        _ => None,
    }
}

/// Fill in name, version and mod id from the jar's metadata; returns its icon's path in the jar.
fn parse_mod(zip: &mut zip::ZipArchive<std::fs::File>, meta: &mut FileMeta) -> Option<String> {
    let json = |zip: &mut zip::ZipArchive<std::fs::File>, name| {
        zip_bytes(zip, name).and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    };
    if let Some(f) = json(zip, "fabric.mod.json") {
        meta.name = f["name"].as_str().map(String::from);
        meta.version = f["version"].as_str().map(String::from);
        meta.mod_id = f["id"].as_str().map(String::from);
        fabric_icon(&f["icon"])
    } else if let Some(q) = json(zip, "quilt.mod.json") {
        let l = &q["quilt_loader"];
        meta.name = l["metadata"]["name"].as_str().map(String::from);
        meta.version = l["version"].as_str().map(String::from);
        meta.mod_id = l["id"].as_str().map(String::from);
        fabric_icon(&l["metadata"]["icon"])
    } else if let Some(t) = ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"]
        .iter()
        .find_map(|n| zip_bytes(zip, n).and_then(|b| toml::from_str::<toml::Value>(&String::from_utf8_lossy(&b)).ok()))
    {
        // Forge 1.13+ / NeoForge. Versions like "${file.jarVersion}" are
        // filled in at build time from the manifest.
        let m = t.get("mods").and_then(|m| m.get(0));
        let field = |k: &str| m.and_then(|m| m.get(k)).and_then(|v| v.as_str()).map(String::from);
        meta.name = field("displayName");
        meta.version = field("version").filter(|v| !v.starts_with("${")).or_else(|| manifest_version(zip));
        meta.mod_id = field("modId");
        field("logoFile").or_else(|| t.get("logoFile").and_then(|v| v.as_str()).map(String::from))
    } else if let Some(info) = json(zip, "mcmod.info") {
        // Forge 1.6–1.12: a list of mods, or {"modList": [...]}.
        let m = if info.is_array() { &info[0] } else { &info["modList"][0] };
        meta.name = m["name"].as_str().map(String::from);
        meta.version = m["version"].as_str().filter(|v| !v.contains("${")).map(String::from);
        meta.mod_id = m["modid"].as_str().map(String::from);
        m["logoFile"].as_str().filter(|p| !p.is_empty()).map(String::from)
    } else {
        None
    }
}

/// `Implementation-Version` from the jar's manifest.
fn manifest_version(zip: &mut zip::ZipArchive<std::fs::File>) -> Option<String> {
    let text = String::from_utf8(zip_bytes(zip, "META-INF/MANIFEST.MF")?).ok()?;
    text.lines().find_map(|l| l.strip_prefix("Implementation-Version:")).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn read_mod(zip: &mut zip::ZipArchive<std::fs::File>, key: &str, meta: &mut FileMeta) {
    if let Some(p) = parse_mod(zip, meta)
        && let Some(bytes) = zip_bytes(zip, &p)
    {
        meta.icon = save_icon(key, ext_of(&p), &bytes);
    }
}

/// A mod jar's own id and version (`sodium`, `0.6.0`), read from its metadata only:
/// no hashing, icons or network. None if the jar names no id.
pub fn mod_identity(path: &Path) -> Option<(String, Option<String>)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let mut meta = FileMeta::default();
    parse_mod(&mut zip, &mut meta);
    Some((meta.mod_id.filter(|id| !id.is_empty())?, meta.version))
}

/// Everything we can learn without the network. `key` identifies the file
/// version (path, size, mtime) for the icon cache.
pub fn read_local(path: &Path, is_dir: bool, kind: ContentKind) -> FileMeta {
    let key = {
        let m = std::fs::metadata(path).ok();
        let stamp = m.as_ref().and_then(|m| m.modified().ok()).map(|t| format!("{t:?}")).unwrap_or_default();
        format!("{}|{}|{stamp}", path.display(), m.map(|m| m.len()).unwrap_or(0))
    };
    let mut meta = FileMeta::default();
    if is_dir {
        if kind == ContentKind::ResourcePacks
            && let Ok(bytes) = std::fs::read(path.join("pack.png"))
        {
            meta.icon = save_icon(&key, "png", &bytes);
        }
        return meta;
    }
    meta.sha1 = net::sha1_file(path);
    let Ok(mut zip) = std::fs::File::open(path).map_err(anyhow::Error::from).and_then(|f| Ok(zip::ZipArchive::new(f)?)) else {
        return meta;
    };
    match kind {
        ContentKind::Mods => read_mod(&mut zip, &key, &mut meta),
        ContentKind::ResourcePacks => {
            if let Some(bytes) = zip_bytes(&mut zip, "pack.png") {
                meta.icon = save_icon(&key, "png", &bytes);
            }
        }
        ContentKind::ShaderPacks => {}
    }
    meta
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub title: String,
    pub icon_url: Option<String>,
}

/// Modrinth's versions for these sha1s, keyed by hash. Unknown files are left out.
async fn version_files<T: serde::de::DeserializeOwned>(hashes: &[String]) -> anyhow::Result<HashMap<String, T>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(net::client()
        .post("https://api.modrinth.com/v2/version_files")
        .json(&serde_json::json!({ "hashes": hashes, "algorithm": "sha1" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// sha1 → the Modrinth project that file belongs to.
pub async fn lookup(hashes: Vec<String>) -> anyhow::Result<HashMap<String, Project>> {
    #[derive(Deserialize)]
    struct V {
        project_id: String,
    }
    #[derive(Deserialize)]
    struct P {
        id: String,
        title: String,
        icon_url: Option<String>,
    }
    let versions: HashMap<String, V> = version_files(&hashes).await?;
    let mut ids: Vec<&str> = versions.values().map(|v| v.project_id.as_str()).collect();
    ids.sort();
    ids.dedup();
    let mut projects = HashMap::new();
    for chunk in ids.chunks(100) {
        let list = serde_json::to_string(chunk)?;
        let url = format!("https://api.modrinth.com/v2/projects?ids={}", urlencode(&list));
        let ps: Vec<P> = net::get_json(&url).await?;
        projects.extend(ps.into_iter().map(|p| (p.id.clone(), Project { id: p.id, title: p.title, icon_url: p.icon_url })));
    }
    Ok(versions.into_iter().filter_map(|(h, v)| Some((h, projects.get(&v.project_id)?.clone()))).collect())
}

/// sha1 → (project id, version id) of the Modrinth version that file is.
pub async fn versions(hashes: Vec<String>) -> anyhow::Result<HashMap<String, (String, String)>> {
    #[derive(Deserialize)]
    struct V {
        id: String,
        project_id: String,
    }
    let mut out = HashMap::new();
    for chunk in hashes.chunks(500) {
        let vs: HashMap<String, V> = version_files(chunk).await?;
        out.extend(vs.into_iter().map(|(h, v)| (h, (v.project_id, v.id))));
    }
    Ok(out)
}

/// A file Modrinth hosts, as a modpack lists it.
#[derive(Debug, Clone)]
pub struct Hosted {
    pub url: String,
    pub sha512: String,
    pub size: u64,
}

/// sha1 → where Modrinth hosts exactly that file.
pub async fn hosted(hashes: Vec<String>) -> anyhow::Result<HashMap<String, Hosted>> {
    #[derive(Deserialize)]
    struct V {
        files: Vec<F>,
    }
    #[derive(Deserialize)]
    struct F {
        url: String,
        size: u64,
        hashes: HashMap<String, String>,
    }
    let mut out = HashMap::new();
    for chunk in hashes.chunks(500) {
        let versions: HashMap<String, V> = version_files(chunk).await?;
        for (sha1, v) in versions {
            // A version can have several files; take the one with this hash.
            let Some(f) = v.files.into_iter().find(|f| f.hashes.get("sha1") == Some(&sha1)) else { continue };
            let Some(sha512) = f.hashes.get("sha512").cloned() else { continue };
            out.insert(sha1, Hosted { url: f.url, sha512, size: f.size });
        }
    }
    Ok(out)
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Download an icon into the cache (once) and return its path.
pub async fn fetch_icon(url: &str) -> anyhow::Result<PathBuf> {
    let file = url.split(['?', '#']).next().unwrap_or(url);
    let path = icon_dir().join(format!("{}.{}", net::short_hash(url.as_bytes()), ext_of(file)));
    if !path.exists() {
        net::download(&Download { url: url.to_string(), path: path.clone(), sha1: None, size: None }).await?;
    }
    Ok(path)
}
