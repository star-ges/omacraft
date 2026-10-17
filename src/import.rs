//! Importing instances from elsewhere: Prism/MultiMC/PolyMC instances (a
//! folder or an exported zip) and Modrinth packs (`.mrpack`, file or URL).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::DateTime;
use serde::Deserialize;

use crate::instance::Instance;
use crate::loaders::{Loader, LoaderKind};
use crate::net::{self, Download};
use crate::task::Task;
use crate::{fsutil, icons, paths};

// -- other launchers on this machine -------------------------------------------

#[derive(Debug, Clone)]
pub struct Found {
    pub launcher: &'static str,
    pub dir: PathBuf,
    pub name: String,
    pub mc_version: String,
    /// Why we can't import it, if we can't.
    pub unsupported: Option<String>,
    pub loader: String,
}

fn launcher_roots() -> Vec<(&'static str, PathBuf)> {
    let home = dirs::home_dir().unwrap_or_default();
    let data = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));
    vec![
        ("Prism", data.join("PrismLauncher")),
        ("Prism", home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher")),
        ("PolyMC", data.join("PolyMC")),
        ("MultiMC", data.join("multimc")),
        ("MultiMC", home.join("MultiMC")),
    ]
}

/// The instances folder, honouring a custom `InstanceDir` in the launcher config.
fn instances_dir(root: &Path) -> PathBuf {
    let cfg = ["prismlauncher.cfg", "polymc.cfg", "multimc.cfg"]
        .iter()
        .find_map(|f| std::fs::read_to_string(root.join(f)).ok())
        .map(|s| parse_cfg(&s))
        .unwrap_or_default();
    match cfg.get("InstanceDir").filter(|d| !d.is_empty()) {
        Some(d) if Path::new(d).is_absolute() => PathBuf::from(d),
        Some(d) => root.join(d),
        None => root.join("instances"),
    }
}

pub fn detect() -> Vec<Found> {
    detect_in(launcher_roots())
}

fn detect_in(roots: Vec<(&'static str, PathBuf)>) -> Vec<Found> {
    let mut out = Vec::new();
    for (launcher, root) in roots {
        let dir = instances_dir(&root);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Ok(p) = PrismInstance::read_dir(&e.path()) {
                out.push(Found {
                    launcher,
                    dir: e.path(),
                    name: p.name.clone(),
                    mc_version: p.mc_version.clone().unwrap_or_default(),
                    loader: p.loader_label(),
                    unsupported: p.loader().err().map(|e| e.to_string()),
                });
            }
        }
    }
    out.sort_by_key(|f| f.name.to_lowercase());
    out
}

// -- Prism / MultiMC format ------------------------------------------------------

/// QSettings INI as written by MultiMC-family launchers.
pub(crate) fn parse_cfg(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in s.lines() {
        if line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let v = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            let inner = &v[1..v.len() - 1];
            let mut out = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => {}
                    Some(c) => out.push(c),
                    None => {}
                }
            }
            out
        } else {
            v.to_string()
        };
        out.insert(k.trim().to_string(), v);
    }
    out
}

#[derive(Deserialize)]
struct MmcPack {
    components: Vec<MmcComponent>,
}

#[derive(Deserialize)]
struct MmcComponent {
    uid: String,
    version: Option<String>,
}

struct PrismInstance {
    cfg: HashMap<String, String>,
    name: String,
    mc_version: Option<String>,
    /// uid → version
    components: Vec<(String, String)>,
}

impl PrismInstance {
    fn parse(cfg: &str, pack: Option<&str>) -> anyhow::Result<Self> {
        let cfg = parse_cfg(cfg);
        let components: Vec<(String, String)> = match pack {
            Some(p) => serde_json::from_str::<MmcPack>(p)
                .context("reading mmc-pack.json")?
                .components
                .into_iter()
                .map(|c| (c.uid, c.version.unwrap_or_default()))
                .collect(),
            None => Vec::new(),
        };
        let mc_version = components
            .iter()
            .find(|(u, _)| u == "net.minecraft")
            .map(|(_, v)| v.clone())
            .or_else(|| cfg.get("IntendedVersion").cloned())
            .filter(|v| !v.is_empty());
        let name = cfg.get("name").cloned().unwrap_or_else(|| "Imported".into());
        Ok(PrismInstance { cfg, name, mc_version, components })
    }

    fn read_dir(dir: &Path) -> anyhow::Result<Self> {
        let cfg = std::fs::read_to_string(dir.join("instance.cfg"))?;
        let pack = std::fs::read_to_string(dir.join("mmc-pack.json")).ok();
        Self::parse(&cfg, pack.as_deref())
    }

    fn component(&self, uid: &str) -> Option<&str> {
        self.components.iter().find(|(u, _)| u == uid).map(|(_, v)| v.as_str())
    }

    fn loader(&self) -> anyhow::Result<Loader> {
        if self.component("com.mumfrey.liteloader").is_some() {
            bail!("LiteLoader isn't supported yet");
        }
        for kind in LoaderKind::ALL {
            if let Some(v) = kind.prism_uid().and_then(|uid| self.component(uid)) {
                return Ok(kind.with(v.into()));
            }
        }
        Ok(Loader::Vanilla)
    }

    fn loader_label(&self) -> String {
        match self.loader() {
            Ok(l) => l.display(),
            Err(_) => "liteloader".into(),
        }
    }

    fn flag(&self, k: &str) -> bool {
        self.cfg.get(k).is_some_and(|v| v == "true")
    }

    /// Build the Omacraft instance (without files).
    fn to_instance(&self) -> anyhow::Result<Instance> {
        let mc = self.mc_version.clone().context("instance has no Minecraft version")?;
        let mut inst = Instance::new(&self.name, &mc, self.loader()?);
        inst.notes = self.cfg.get("notes").cloned().unwrap_or_default();
        inst.played_secs = self.cfg.get("totalTimePlayed").and_then(|v| v.parse().ok()).unwrap_or(0);
        inst.last_played = self
            .cfg
            .get("lastLaunchTime")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&ms| ms > 0)
            .and_then(DateTime::from_timestamp_millis);
        if self.flag("OverrideMemory") {
            inst.memory_mb = self.cfg.get("MaxMemAlloc").and_then(|v| v.parse().ok());
        }
        if self.flag("OverrideJavaArgs") {
            inst.jvm_args = self.cfg.get("JvmArgs").cloned().unwrap_or_default();
        }
        if self.flag("OverrideWindow") {
            let w = self.cfg.get("MinecraftWinWidth").and_then(|v| v.parse().ok());
            let h = self.cfg.get("MinecraftWinHeight").and_then(|v| v.parse().ok());
            if let (Some(w), Some(h)) = (w, h) {
                inst.window = Some((w, h));
            }
        }
        Ok(inst)
    }

    /// The icon key, if it's a plain file name: it's joined onto paths when
    /// looking for a custom icon, so `../x` or `/x` must never get through.
    fn icon_key(&self) -> Option<&str> {
        let k = self.cfg.get("iconKey").map(|s| s.as_str()).filter(|k| *k != "default")?;
        safe_rel(k).filter(|p| p.components().count() == 1 && !k.contains('\0')).map(|_| k)
    }
}

/// Map a Prism icon key to ours, copying custom icon files across.
fn import_icon(key: &str, search: &[PathBuf]) -> String {
    let base = key.trim_end_matches("_legacy");
    if icons::is_builtin(base) {
        return base.to_string();
    }
    for dir in search {
        for ext in ["png", "svg", "jpg", "jpeg"] {
            let p = dir.join(format!("{key}.{ext}"));
            if p.is_file()
                && let Ok(k) = icons::add_custom(&p)
            {
                return k;
            }
        }
    }
    icons::DEFAULT.to_string()
}

/// Group names live beside the instances, in instgroups.json.
fn prism_group(instance_dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Groups {
        groups: HashMap<String, Group>,
    }
    #[derive(Deserialize)]
    struct Group {
        #[serde(default)]
        instances: Vec<String>,
    }
    let id = instance_dir.file_name()?.to_string_lossy().into_owned();
    let s = std::fs::read_to_string(instance_dir.parent()?.join("instgroups.json")).ok()?;
    let g: Groups = serde_json::from_str(&s).ok()?;
    g.groups.into_iter().find(|(_, v)| v.instances.contains(&id)).map(|(k, _)| k)
}

/// The instance a Prism `instance.cfg` and `mmc-pack.json` describe (for export tests).
#[cfg(test)]
pub(crate) fn prism_instance(cfg: &str, pack: Option<&str>) -> anyhow::Result<Instance> {
    PrismInstance::parse(cfg, pack)?.to_instance()
}

// -- Modrinth packs -----------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MrIndex {
    name: String,
    #[serde(default)]
    summary: Option<String>,
    files: Vec<MrFile>,
    dependencies: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MrFile {
    path: String,
    hashes: HashMap<String, String>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    downloads: Vec<String>,
    file_size: Option<u64>,
}

fn mrpack_loader(deps: &HashMap<String, String>) -> Loader {
    deps.iter().find_map(|(k, v)| Loader::from_modrinth(k, v.clone())).unwrap_or_default()
}

/// The files a pack downloads, with paths relative to the game dir. Checked before
/// any instance exists: every file needs a safe path, HTTPS downloads and a SHA-1
/// (the format requires one, and it's what the download is verified against).
fn mrpack_downloads(index: &MrIndex) -> anyhow::Result<Vec<Download>> {
    let mut out = Vec::new();
    for f in &index.files {
        let rel = safe_rel(&f.path).with_context(|| format!("pack has an unsafe path: {}", f.path))?;
        let url = f.downloads.first().with_context(|| format!("no download for {}", f.path))?;
        if let Some(u) = f.downloads.iter().find(|u| reqwest::Url::parse(u).map_or(true, |u| u.scheme() != "https" || u.host().is_none())) {
            bail!("pack downloads {} from a non-HTTPS URL: {u}", f.path);
        }
        let sha1 = f.hashes.get("sha1").filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        let sha1 = sha1.with_context(|| format!("pack has no valid SHA-1 for {}", f.path))?;
        if f.env.as_ref().and_then(|e| e.get("client")).is_some_and(|c| c == "unsupported") {
            continue;
        }
        out.push(Download { url: url.clone(), path: rel, sha1: Some(sha1.clone()), size: f.file_size });
    }
    Ok(out)
}

/// A relative path that stays inside the game dir.
fn safe_rel(p: &str) -> Option<PathBuf> {
    let path = Path::new(p);
    let ok = path.components().all(|c| matches!(c, std::path::Component::Normal(_)));
    (ok && !p.is_empty()).then(|| path.to_path_buf())
}

// -- entry point ---------------------------------------------------------------------

pub struct Request {
    /// File path, folder path, or URL.
    pub source: String,
    pub name: Option<String>,
    pub group: String,
}

pub struct Imported {
    pub inst: Instance,
    /// Things left out along the way (links pointing outside the instance).
    pub warnings: Vec<String>,
}

/// Import into a new instance. On failure, nothing is left behind.
pub async fn import(task: Task, req: Request) -> anyhow::Result<Imported> {
    let src = req.source.trim().trim_start_matches("file://").to_string();
    let (path, downloaded) = if src.starts_with("http://") || src.starts_with("https://") {
        (fetch(&task, &src).await?, true)
    } else {
        let p = PathBuf::from(shellexpand(&src));
        if !p.exists() {
            bail!("{} doesn't exist", p.display());
        }
        (p, false)
    };
    let r = import_path(&task, &path).await;
    if downloaded {
        let _ = std::fs::remove_file(&path);
    }
    let (mut inst, warnings) = r?;
    if let Some(n) = req.name.as_ref().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        inst.name = n.to_string();
    }
    if !req.group.trim().is_empty() {
        inst.group = req.group.trim().to_string();
    }
    inst.save()?;
    task.finish(&Ok(()));
    Ok(Imported { inst, warnings })
}

fn shellexpand(s: &str) -> String {
    match s.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest).to_string_lossy().into_owned(),
        None => s.to_string(),
    }
}

/// What a pack URL points at.
#[derive(Debug, PartialEq)]
enum PackUrl {
    /// A Modrinth project page: its newest version.
    Project(String),
    /// A Modrinth version page, by version ID or number (decoded).
    Version { project: String, version: String },
    /// Anything else is downloaded as is.
    File,
}

fn pack_url(url: &str) -> anyhow::Result<PackUrl> {
    let u = reqwest::Url::parse(url).with_context(|| format!("{url} isn't a valid URL"))?;
    if !matches!(u.host_str(), Some("modrinth.com" | "www.modrinth.com")) {
        return Ok(PackUrl::File);
    }
    // The project stays percent-encoded since it goes back into an API path.
    let seg: Vec<&str> = u.path_segments().into_iter().flatten().filter(|s| !s.is_empty()).collect();
    match seg[..] {
        ["modpack" | "project", project, "version", version, ..] => Ok(PackUrl::Version {
            project: project.to_string(),
            version: percent_encoding::percent_decode_str(version).decode_utf8_lossy().into_owned(),
        }),
        ["modpack" | "project", project, ..] => Ok(PackUrl::Project(project.to_string())),
        _ => bail!("{url} isn't a Modrinth modpack page"),
    }
}

#[derive(Deserialize)]
struct MrVersion {
    id: String,
    version_number: String,
    files: Vec<MrVersionFile>,
}

#[derive(Deserialize)]
struct MrVersionFile {
    url: String,
    primary: bool,
    #[serde(default)]
    hashes: HashMap<String, String>,
    size: Option<u64>,
}

/// The requested version (ID first, then version number), or the newest if none was asked for.
fn pick_version<'a>(versions: &'a [MrVersion], want: Option<&str>) -> Option<&'a MrVersion> {
    match want {
        None => versions.first(),
        Some(w) => versions.iter().find(|v| v.id == w).or_else(|| versions.iter().find(|v| v.version_number == w)),
    }
}

/// Where to download a pack from: Modrinth pages resolve to the .mrpack of the
/// version they name (or the newest), with its SHA-1 to check.
async fn resolve_pack(url: &str, path: PathBuf) -> anyhow::Result<Download> {
    let (project, want) = match pack_url(url)? {
        PackUrl::File => return Ok(Download { url: url.to_string(), path, sha1: None, size: None }),
        PackUrl::Project(p) => (p, None),
        PackUrl::Version { project, version } => (project, Some(version)),
    };
    let versions: Vec<MrVersion> = net::get_json(&format!("https://api.modrinth.com/v2/project/{project}/version")).await?;
    let v = match &want {
        Some(w) => pick_version(&versions, Some(w)).with_context(|| format!("{project} has no version {w}"))?,
        None => pick_version(&versions, None).with_context(|| format!("{project} has no versions"))?,
    };
    let f = v.files.iter().find(|f| f.primary).or(v.files.first()).context("version has no files")?;
    Ok(Download { url: f.url.clone(), path, sha1: f.hashes.get("sha1").cloned(), size: f.size })
}

/// Download a pack to a temporary file.
async fn fetch(task: &Task, url: &str) -> anyhow::Result<PathBuf> {
    task.stage("Downloading pack", 0);
    let dir = paths::data_dir().join("cache");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("import-{}.zip", uuid::Uuid::new_v4()));
    net::download(&resolve_pack(url, path.clone()).await?).await?;
    Ok(path)
}

async fn import_path(task: &Task, path: &Path) -> anyhow::Result<(Instance, Vec<String>)> {
    if path.is_dir() {
        if path.join("instance.cfg").is_file() {
            return import_prism_dir(task, path).await;
        }
        bail!("{} isn't a Prism or MultiMC instance folder (no instance.cfg)", path.display());
    }
    let file = std::fs::File::open(path)?;
    let zip = zip::ZipArchive::new(file).context("not a zip, .mrpack or instance folder")?;
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    if names.iter().any(|n| n == "modrinth.index.json") {
        return import_mrpack(task, path.to_path_buf()).await;
    }
    if let Some(cfg) = names.iter().filter(|n| n.ends_with("instance.cfg")).min_by_key(|n| n.len()) {
        let prefix = cfg.trim_end_matches("instance.cfg").to_string();
        return import_prism_zip(task, path.to_path_buf(), prefix).await;
    }
    if names.iter().any(|n| n == "manifest.json") {
        bail!("CurseForge packs aren't supported yet");
    }
    bail!("couldn't recognise {} as an instance or modpack", path.display())
}

/// Claim a new instance directory, run `fill` (which returns any warnings), and
/// clean up on failure. The directory is ours alone, so removing it is safe.
async fn with_new_instance<F, Fut>(mut inst: Instance, fill: F) -> anyhow::Result<(Instance, Vec<String>)>
where
    F: FnOnce(Instance) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Vec<String>>>,
{
    inst.assign_id()?;
    let r = match inst.save() {
        Ok(()) => fill(inst.clone()).await,
        Err(e) => Err(e),
    };
    match r {
        Ok(warnings) => Ok((inst, warnings)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(inst.dir());
            Err(e)
        }
    }
}

async fn import_prism_dir(task: &Task, dir: &Path) -> anyhow::Result<(Instance, Vec<String>)> {
    let p = PrismInstance::read_dir(dir)?;
    let mut inst = p.to_instance()?;
    inst.group = prism_group(dir).unwrap_or_default();
    if let Some(key) = p.icon_key() {
        let mut search = vec![dir.to_path_buf()];
        if let Some(root) = dir.parent().and_then(|d| d.parent()) {
            search.push(root.join("icons"));
        }
        inst.icon = import_icon(key, &search);
    }
    let src = [dir.join("minecraft"), dir.join(".minecraft")].into_iter().find(|d| d.is_dir());
    let task = task.clone();
    with_new_instance(inst, |inst| async move {
        let Some(src) = src else { return Ok(Vec::new()) };
        let dst = inst.game_dir();
        tokio::task::spawn_blocking(move || {
            task.stage("Measuring instance", 0);
            let total = fsutil::tree_size(&src);
            task.stage(format!("Copying {}", inst.name), total.max(1));
            fsutil::copy_tree(&src, &dst, &|n| task.inc(n)).context("copying instance files")
        })
        .await?
    })
    .await
}

fn zip_text(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> anyhow::Result<String> {
    let bytes = fsutil::read_capped(zip.by_name(name)?).with_context(|| format!("reading {name}"))?;
    Ok(String::from_utf8(bytes).with_context(|| format!("reading {name}"))?)
}

/// Extract entries under `prefix` into `dst`, returning how many were written.
fn extract(zip: &mut zip::ZipArchive<std::fs::File>, prefix: &str, dst: &Path, task: &Task) -> anyhow::Result<u64> {
    let mut n = 0;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i)?;
        let name = e.name()?.to_string();
        let Some(rel) = name.strip_prefix(prefix) else { continue };
        let Some(rel) = safe_rel(rel.trim_end_matches('/')) else { continue };
        let out = dst.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        std::fs::create_dir_all(out.parent().unwrap())?;
        std::io::copy(&mut e, &mut std::fs::File::create(&out)?)?;
        n += 1;
        task.inc(1);
    }
    Ok(n)
}

async fn import_prism_zip(task: &Task, zip_path: PathBuf, prefix: String) -> anyhow::Result<(Instance, Vec<String>)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path)?)?;
    let cfg = zip_text(&mut zip, &format!("{prefix}instance.cfg"))?;
    let pack = zip_text(&mut zip, &format!("{prefix}mmc-pack.json")).ok();
    let p = PrismInstance::parse(&cfg, pack.as_deref())?;
    let mut inst = p.to_instance()?;
    // Exports carry custom icons beside instance.cfg.
    if let Some(key) = p.icon_key() {
        let tmp = paths::data_dir().join("cache").join(format!("icon-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp)?;
        for ext in ["png", "svg", "jpg", "jpeg"] {
            if let Ok(e) = zip.by_name(&format!("{prefix}{key}.{ext}")) {
                let bytes = fsutil::read_capped(e).with_context(|| format!("reading icon {key}.{ext}"))?;
                std::fs::write(tmp.join(format!("{key}.{ext}")), bytes)?;
            }
        }
        inst.icon = import_icon(key, &[tmp.clone()]);
        let _ = std::fs::remove_dir_all(tmp);
    }
    let dot = format!("{prefix}.minecraft/");
    let game = if zip.file_names().flatten().any(|n| n.starts_with(&dot)) { ".minecraft/" } else { "minecraft/" };
    let task = task.clone();
    with_new_instance(inst, |inst| async move {
        let dst = inst.game_dir();
        tokio::task::spawn_blocking(move || {
            let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path)?)?;
            task.stage(format!("Extracting {}", inst.name), zip.len() as u64);
            extract(&mut zip, &format!("{prefix}{game}"), &dst, &task).map(|_| Vec::new())
        })
        .await?
    })
    .await
}

async fn import_mrpack(task: &Task, zip_path: PathBuf) -> anyhow::Result<(Instance, Vec<String>)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path)?)?;
    let index: MrIndex = serde_json::from_str(&zip_text(&mut zip, "modrinth.index.json")?).context("reading modrinth.index.json")?;
    let mc = index.dependencies.get("minecraft").context("pack doesn't say which Minecraft version it needs")?;
    let mut inst = Instance::new(&index.name, mc, mrpack_loader(&index.dependencies));
    inst.icon = "modrinth".into();
    inst.notes = index.summary.clone().unwrap_or_default();
    let mut downloads = mrpack_downloads(&index)?;
    let task = task.clone();
    with_new_instance(inst, |inst| async move {
        let game = inst.game_dir();
        for d in &mut downloads {
            d.path = game.join(&d.path);
        }
        net::fetch_all(&task, "Downloading pack files", downloads, 8, async |d| crate::store::fetch(&d).await).await?;
        let t = task.clone();
        tokio::task::spawn_blocking(move || {
            let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path)?)?;
            t.stage("Applying overrides", zip.len() as u64);
            extract(&mut zip, "overrides/", &game, &t)?;
            extract(&mut zip, "client-overrides/", &game, &t)?;
            anyhow::Ok(Vec::new())
        })
        .await?
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qsettings_values() {
        let cfg = parse_cfg("[General]\nname=My Pack\nnotes=\"line one\\nline \\\"two\\\"\"\nOverrideMemory=true\nMaxMemAlloc=6144\n");
        assert_eq!(cfg["name"], "My Pack");
        assert_eq!(cfg["notes"], "line one\nline \"two\"");
        assert_eq!(cfg["MaxMemAlloc"], "6144");
    }

    #[test]
    fn reads_prism_components() {
        let pack = r#"{"components":[{"uid":"org.lwjgl3","version":"3.3.3"},{"uid":"net.minecraft","version":"1.21.1"},{"uid":"net.fabricmc.intermediary","version":"1.21.1"},{"uid":"net.fabricmc.fabric-loader","version":"0.16.5"}]}"#;
        let p = PrismInstance::parse("name=X\nOverrideMemory=true\nMaxMemAlloc=6144\ntotalTimePlayed=3600\nlastLaunchTime=1789824260016\n", Some(pack)).unwrap();
        let i = p.to_instance().unwrap();
        assert_eq!(i.mc_version, "1.21.1");
        assert_eq!(i.loader, Loader::Fabric("0.16.5".into()));
        assert_eq!(i.memory_mb, Some(6144));
        assert_eq!(i.played_secs, 3600);
        assert!(i.last_played.is_some());

        let forge = r#"{"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.minecraftforge","version":"47.2.0"}]}"#;
        assert_eq!(PrismInstance::parse("name=F\n", Some(forge)).unwrap().to_instance().unwrap().loader, Loader::Forge("47.2.0".into()));
        let lite = r#"{"components":[{"uid":"net.minecraft","version":"1.12.2"},{"uid":"com.mumfrey.liteloader","version":"1.12.2"}]}"#;
        assert!(PrismInstance::parse("name=L\n", Some(lite)).unwrap().to_instance().is_err());
    }

    #[test]
    fn rejects_escaping_paths() {
        assert!(safe_rel("mods/a.jar").is_some());
        assert!(safe_rel("../a.jar").is_none());
        assert!(safe_rel("/etc/passwd").is_none());
        assert!(safe_rel("mods/../../a").is_none());
    }

    fn scratch() -> PathBuf {
        let d = paths::test_root().join(format!("import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const PACK: &str = r#"{"components":[{"uid":"net.minecraft","version":"1.21.1"}]}"#;

    #[test]
    fn detects_launcher_instances() {
        let root = scratch().join("PrismLauncher");
        let inst = root.join("instances/fab");
        std::fs::create_dir_all(&inst).unwrap();
        std::fs::write(inst.join("instance.cfg"), "name=Fab\n").unwrap();
        std::fs::write(inst.join("mmc-pack.json"), PACK).unwrap();
        std::fs::create_dir_all(root.join("instances/not-an-instance")).unwrap();
        let found = detect_in(vec![("Prism", root.clone())]);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].name.as_str(), found[0].mc_version.as_str()), ("Fab", "1.21.1"));
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn icon_key_must_be_a_file_name() {
        let key = |k: &str| PrismInstance::parse(&format!("iconKey={k}\n"), None).unwrap().icon_key().map(String::from);
        assert_eq!(key("flame").as_deref(), Some("flame"));
        for bad in ["/tmp/x", "../x", "a/b", "..", ".", "", "default"] {
            assert_eq!(key(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn zip_icon_key_cannot_escape() {
        use std::io::Write;
        let d = scratch();
        let outside = d.join("outside");
        std::fs::write(outside.with_extension("png"), "marker").unwrap();
        let zip_path = d.join("evil.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("instance.cfg", opts).unwrap();
        write!(z, "name=Evil\niconKey={}\n", outside.display()).unwrap();
        z.start_file("mmc-pack.json", opts).unwrap();
        z.write_all(PACK.as_bytes()).unwrap();
        z.start_file(format!("{}.png", outside.display()), opts).unwrap();
        z.write_all(b"pwned").unwrap();
        z.start_file("minecraft/options.txt", opts).unwrap();
        z.write_all(b"ok").unwrap();
        z.finish().unwrap();

        let req = Request { source: zip_path.display().to_string(), name: None, group: String::new() };
        let r = import(Task::new("t"), req).await.unwrap();
        assert_eq!(std::fs::read_to_string(outside.with_extension("png")).unwrap(), "marker");
        assert_eq!(r.inst.icon, icons::DEFAULT);
        assert!(r.inst.game_dir().join("options.txt").is_file());
        std::fs::remove_dir_all(r.inst.dir()).unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn tells_modrinth_versions_from_projects() {
        let v = |p: &str, v: &str| PackUrl::Version { project: p.into(), version: v.into() };
        assert_eq!(pack_url("https://modrinth.com/modpack/fo").unwrap(), PackUrl::Project("fo".into()));
        assert_eq!(pack_url("https://www.modrinth.com/modpack/fo/versions?c=release#top").unwrap(), PackUrl::Project("fo".into()));
        assert_eq!(pack_url("https://modrinth.com/modpack/fo/version/6.4.0").unwrap(), v("fo", "6.4.0"));
        assert_eq!(pack_url("https://modrinth.com/project/fo/version/uQxD0ENB/").unwrap(), v("fo", "uQxD0ENB"));
        assert_eq!(pack_url("https://modrinth.com/modpack/fo/version/1.0%20beta").unwrap(), v("fo", "1.0 beta"));
        assert_eq!(pack_url("https://cdn.modrinth.com/data/x/versions/y/pack.mrpack").unwrap(), PackUrl::File);
        assert_eq!(pack_url("https://example.com/modpack/fo/version/1").unwrap(), PackUrl::File);
        assert!(pack_url("https://modrinth.com/mod/sodium").is_err());
    }

    #[test]
    fn picks_the_requested_version() {
        let mk = |id: &str, n: &str| MrVersion { id: id.into(), version_number: n.into(), files: Vec::new() };
        let vs = [mk("new", "2.0"), mk("old", "1.0"), mk("2.0", "0.1")];
        assert_eq!(pick_version(&vs, None).unwrap().id, "new");
        assert_eq!(pick_version(&vs, Some("1.0")).unwrap().id, "old");
        assert_eq!(pick_version(&vs, Some("old")).unwrap().id, "old");
        assert_eq!(pick_version(&vs, Some("2.0")).unwrap().id, "2.0", "an ID match wins over a version number");
        assert!(pick_version(&vs, Some("3.0")).is_none());
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn resolves_modrinth_version_urls() {
        let p = PathBuf::from("x");
        let d = resolve_pack("https://modrinth.com/modpack/fabulously-optimized/version/1.12.1", p.clone()).await.unwrap();
        assert_eq!(d.sha1.as_deref(), Some("bd8cffe21cd22dbe9ea0e4555d163178cba6c19e"));
        assert!(d.url.ends_with(".mrpack"), "{}", d.url);
        let err = resolve_pack("https://modrinth.com/modpack/fabulously-optimized/version/0.0.0-nope", p).await.unwrap_err();
        assert!(err.to_string().contains("no version 0.0.0-nope"), "{err}");
    }

    const SHA1: &str = "bd8cffe21cd22dbe9ea0e4555d163178cba6c19e";

    fn mrfile(url: &str, hashes: &str) -> String {
        format!(r#"{{"path":"mods/a.jar","hashes":{hashes},"downloads":["{url}"],"fileSize":1}}"#)
    }

    fn index(files: &[String]) -> MrIndex {
        let json = format!(r#"{{"name":"P","files":[{}],"dependencies":{{"minecraft":"1.21.1"}}}}"#, files.join(","));
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn mrpack_files_need_https_and_sha1() {
        let ok = mrfile("https://cdn.modrinth.com/a.jar", &format!(r#"{{"sha1":"{SHA1}"}}"#));
        let d = mrpack_downloads(&index(&[ok])).unwrap();
        assert_eq!((d[0].path.as_path(), d[0].sha1.as_deref()), (Path::new("mods/a.jar"), Some(SHA1)));

        let bad = [
            mrfile("http://cdn.modrinth.com/a.jar", &format!(r#"{{"sha1":"{SHA1}"}}"#)),
            mrfile("file:///etc/passwd", &format!(r#"{{"sha1":"{SHA1}"}}"#)),
            mrfile("https://cdn.modrinth.com/a.jar", "{}"),
            mrfile("https://cdn.modrinth.com/a.jar", r#"{"sha512":"00"}"#),
            mrfile("https://cdn.modrinth.com/a.jar", r#"{"sha1":"not-a-hash"}"#),
        ];
        for f in bad {
            assert!(mrpack_downloads(&index(std::slice::from_ref(&f))).is_err(), "{f}");
        }
    }

    #[tokio::test]
    async fn bad_mrpack_leaves_no_instance() {
        use std::io::Write;
        let d = scratch();
        let name = format!("Rejected {}", uuid::Uuid::new_v4());
        let path = d.join("bad.mrpack");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        z.start_file("modrinth.index.json", zip::write::SimpleFileOptions::default()).unwrap();
        let file = mrfile("http://example.com/a.jar", "{}");
        write!(z, r#"{{"name":"{name}","files":[{file}],"dependencies":{{"minecraft":"1.21.1"}}}}"#).unwrap();
        z.finish().unwrap();

        let req = Request { source: path.display().to_string(), name: None, group: String::new() };
        let Err(err) = import(Task::new("t"), req).await else { panic!("bad pack imported") };
        assert!(err.to_string().contains("non-HTTPS"), "{err}");
        assert!(!crate::instance::load_all().iter().any(|i| i.name == name));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn oversized_metadata_fails_cleanly() {
        use std::io::Write;
        let d = scratch();
        let path = d.join("bomb.mrpack");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        z.start_file("modrinth.index.json", zip::write::SimpleFileOptions::default()).unwrap();
        let spaces = vec![b' '; 1 << 20];
        for _ in 0..=fsutil::METADATA_LIMIT >> 20 {
            z.write_all(&spaces).unwrap();
        }
        z.finish().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 1 << 20);

        let req = Request { source: path.display().to_string(), name: None, group: String::new() };
        let Err(err) = import(Task::new("t"), req).await else { panic!("oversized index imported") };
        assert!(format!("{err:#}").contains("larger than 16 MiB"), "{err:#}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn folder_import_is_self_contained() {
        let d = scratch();
        let (src, elsewhere) = (d.join("inst"), d.join("elsewhere"));
        std::fs::create_dir_all(src.join("minecraft/real-saves/W")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(src.join("instance.cfg"), "name=Linked\n").unwrap();
        std::fs::write(src.join("mmc-pack.json"), PACK).unwrap();
        std::fs::write(src.join("minecraft/real-saves/W/level.dat"), "w").unwrap();
        std::os::unix::fs::symlink("real-saves", src.join("minecraft/saves")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, src.join("minecraft/screenshots")).unwrap();

        let req = Request { source: src.display().to_string(), name: None, group: String::new() };
        let r = import(Task::new("t"), req).await.unwrap();
        let saves = r.inst.saves_dir();
        assert!(!std::fs::symlink_metadata(&saves).unwrap().is_symlink());
        assert!(saves.join("W/level.dat").is_file());
        std::fs::write(elsewhere.join("shot.png"), "png").unwrap();
        assert!(!std::fs::symlink_metadata(r.inst.screenshots_dir()).unwrap().is_symlink());
        assert!(!r.inst.screenshots_dir().join("shot.png").exists());
        std::fs::remove_dir_all(r.inst.saves_dir().join("W")).unwrap();
        assert!(src.join("minecraft/real-saves/W/level.dat").is_file());
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        std::fs::remove_dir_all(r.inst.dir()).unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }
}
