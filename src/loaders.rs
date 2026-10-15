//! Mod loaders. Fabric and Quilt publish ready-made launcher profiles that
//! layer straight onto the vanilla version JSON. Forge and NeoForge come from
//! Prism's meta, in the same shape: 1.6–1.12 Forge runs through launchwrapper
//! with a tweak class, and 1.13+ through ForgeWrapper, which runs the
//! installer's processors (patching the client jar) on first launch.

use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::mojang::VersionJson;
use crate::{net, paths};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "version", rename_all = "lowercase")]
pub enum Loader {
    #[default]
    Vanilla,
    Fabric(String),
    Quilt(String),
    Forge(String),
    NeoForge(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

const PRISM_META: &str = "https://meta.prismlauncher.org/v1";

/// ForgeWrapper's entry point; launch adds the `-Dforgewrapper.*` paths it needs.
pub const FORGE_WRAPPER_MAIN: &str = "io.github.zekerzhayard.forgewrapper.installer.Main";

impl LoaderKind {
    pub const ALL: [LoaderKind; 5] = [LoaderKind::Vanilla, LoaderKind::Fabric, LoaderKind::Quilt, LoaderKind::Forge, LoaderKind::NeoForge];

    pub fn label(self) -> &'static str {
        match self {
            LoaderKind::Vanilla => "vanilla",
            LoaderKind::Fabric => "fabric",
            LoaderKind::Quilt => "quilt",
            LoaderKind::Forge => "forge",
            LoaderKind::NeoForge => "neoforge",
        }
    }

    pub fn parse(s: &str) -> Option<LoaderKind> {
        LoaderKind::ALL.into_iter().find(|k| k.label() == s)
    }

    /// The loader of this kind at `version` (ignored for vanilla).
    pub fn with(self, version: String) -> Loader {
        match self {
            LoaderKind::Vanilla => Loader::Vanilla,
            LoaderKind::Fabric => Loader::Fabric(version),
            LoaderKind::Quilt => Loader::Quilt(version),
            LoaderKind::Forge => Loader::Forge(version),
            LoaderKind::NeoForge => Loader::NeoForge(version),
        }
    }

    /// Fabric-style meta server (loader profiles by Minecraft version).
    fn meta(self) -> Option<&'static str> {
        match self {
            LoaderKind::Fabric => Some("https://meta.fabricmc.net/v2"),
            LoaderKind::Quilt => Some("https://meta.quiltmc.org/v3"),
            _ => None,
        }
    }

    /// Component uid in Prism's meta (and in Prism instances).
    pub fn prism_uid(self) -> Option<&'static str> {
        match self {
            LoaderKind::Vanilla => None,
            LoaderKind::Fabric => Some("net.fabricmc.fabric-loader"),
            LoaderKind::Quilt => Some("org.quiltmc.quilt-loader"),
            LoaderKind::Forge => Some("net.minecraftforge"),
            LoaderKind::NeoForge => Some("net.neoforged"),
        }
    }

    /// Name in Modrinth's facets and `.mrpack` dependencies.
    pub fn modrinth(self) -> Option<&'static str> {
        match self {
            LoaderKind::Vanilla => None,
            k => Some(k.label()),
        }
    }
}

impl Loader {
    pub fn kind(&self) -> LoaderKind {
        match self {
            Loader::Vanilla => LoaderKind::Vanilla,
            Loader::Fabric(_) => LoaderKind::Fabric,
            Loader::Quilt(_) => LoaderKind::Quilt,
            Loader::Forge(_) => LoaderKind::Forge,
            Loader::NeoForge(_) => LoaderKind::NeoForge,
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Loader::Vanilla => None,
            Loader::Fabric(v) | Loader::Quilt(v) | Loader::Forge(v) | Loader::NeoForge(v) => Some(v),
        }
    }

    pub fn display(&self) -> String {
        match self.version() {
            Some(v) => format!("{} {v}", self.kind().label()),
            None => "vanilla".into(),
        }
    }

    /// Modrinth's name for this loader.
    pub fn modrinth(&self) -> Option<&'static str> {
        self.kind().modrinth()
    }

    /// The loader for a Modrinth/`.mrpack` loader name.
    pub fn from_modrinth(name: &str, version: String) -> Option<Loader> {
        let kind = match name {
            "fabric-loader" | "fabric" => LoaderKind::Fabric,
            "quilt-loader" | "quilt" => LoaderKind::Quilt,
            "forge" => LoaderKind::Forge,
            "neoforge" => LoaderKind::NeoForge,
            _ => return None,
        };
        Some(kind.with(version))
    }
}

/// One loader build offered for a Minecraft version.
#[derive(Debug, Clone)]
pub struct Build {
    pub version: String,
    /// Shown beside the version: "stable", "beta", "recommended"…
    pub tag: &'static str,
    /// Picked by default (the first preferred build, else the newest).
    pub preferred: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct LoaderEntry {
    loader: LoaderVersion,
}

#[derive(Debug, Clone, Deserialize)]
struct LoaderVersion {
    version: String,
    #[serde(default)]
    stable: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct PrismIndex {
    versions: Vec<PrismIndexEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct PrismIndexEntry {
    version: String,
    #[serde(default)]
    recommended: bool,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    requires: Vec<PrismRequire>,
}

#[derive(Debug, Clone, Deserialize)]
struct PrismRequire {
    uid: String,
    #[serde(default)]
    equals: Option<String>,
}

/// Forge before 1.6 patched the client jar itself (jar mods), which we
/// don't do. Snapshots and anything unrecognised count as newer.
fn is_jar_mod_era(mc: &str) -> bool {
    let mut parts = mc.split('.').map(|p| p.parse::<u32>().ok());
    matches!((parts.next(), parts.next()), (Some(Some(1)), Some(Some(minor))) if minor < 6)
}

/// Forge's index covers every Minecraft version (2 MB), so fetch it once a session.
static PRISM_INDEXES: std::sync::Mutex<Vec<(LoaderKind, Arc<Vec<PrismIndexEntry>>)>> = std::sync::Mutex::new(Vec::new());

async fn prism_index(kind: LoaderKind, uid: &str) -> anyhow::Result<Arc<Vec<PrismIndexEntry>>> {
    if let Some((_, i)) = PRISM_INDEXES.lock().unwrap().iter().find(|(k, _)| *k == kind) {
        return Ok(i.clone());
    }
    let url = format!("{PRISM_META}/{uid}/index.json");
    let cache = paths::meta_cache_dir().join(kind.label()).join("index.json");
    let index: PrismIndex = net::get_json_cached(&url, &cache).await?;
    let index = Arc::new(index.versions);
    PRISM_INDEXES.lock().unwrap().push((kind, index.clone()));
    Ok(index)
}

/// Loader versions available for a Minecraft version, newest first.
pub async fn versions_for(kind: LoaderKind, mc: &str) -> anyhow::Result<Vec<Build>> {
    if let Some(base) = kind.meta() {
        let url = format!("{base}/versions/loader/{mc}");
        let cache = paths::meta_cache_dir().join(kind.label()).join(format!("{mc}.json"));
        let list: Vec<LoaderEntry> = net::get_json_cached(&url, &cache).await?;
        return Ok(list
            .into_iter()
            .map(|e| {
                // Quilt marks betas in the version string instead of a flag.
                let stable = e.loader.stable.unwrap_or(!e.loader.version.contains("beta"));
                Build { version: e.loader.version, tag: if stable { "stable" } else { "beta" }, preferred: stable }
            })
            .collect());
    }
    let Some(uid) = kind.prism_uid() else {
        return Ok(vec![]);
    };
    if kind == LoaderKind::Forge && is_jar_mod_era(mc) {
        anyhow::bail!("Forge before Minecraft 1.6 isn't supported");
    }
    Ok(prism_index(kind, uid)
        .await?
        .iter()
        .cloned()
        .filter(|v| v.requires.iter().any(|r| r.uid == "net.minecraft" && r.equals.as_deref() == Some(mc)))
        .map(|v| {
            // Forge flags one build per Minecraft version as recommended;
            // NeoForge marks betas by type instead.
            let beta = v.kind.as_deref() == Some("beta");
            let tag = if v.recommended {
                "recommended"
            } else if beta {
                "beta"
            } else {
                ""
            };
            Build { version: v.version, tag, preferred: v.recommended || (kind == LoaderKind::NeoForge && !beta) }
        })
        .collect())
}

/// A Forge/NeoForge component from Prism's meta, as a child version JSON.
async fn prism_profile(kind: LoaderKind, ver: &str, mc: &str) -> anyhow::Result<VersionJson> {
    let uid = kind.prism_uid().unwrap_or_default();
    let id = format!("{}-{ver}", kind.label());
    let cache = paths::versions_dir().join(&id).join(format!("{id}.json"));
    let raw: serde_json::Value = match tokio::fs::read(&cache).await.ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(v) => v,
        None => net::get_json_cached(&format!("{PRISM_META}/{uid}/{ver}.json"), &cache).await?,
    };
    if raw.get("jarMods").is_some() {
        anyhow::bail!("Forge for Minecraft {mc} is a jar mod, which isn't supported (Forge 1.6 and later is)");
    }
    let mut v: VersionJson = serde_json::from_value(raw).with_context(|| format!("reading {id}"))?;
    v.id = format!("{id}-{mc}");
    Ok(v)
}

pub async fn profile(loader: &Loader, mc: &str) -> anyhow::Result<Option<VersionJson>> {
    if let (LoaderKind::Forge | LoaderKind::NeoForge, Some(ver)) = (loader.kind(), loader.version()) {
        return Ok(Some(prism_profile(loader.kind(), ver, mc).await?));
    }
    let (Some(base), Some(ver)) = (loader.kind().meta(), loader.version()) else {
        return Ok(None);
    };
    let url = format!("{base}/versions/loader/{mc}/{ver}/profile/json");
    let id = format!("{}-{ver}-{mc}", loader.kind().label());
    let cache = paths::versions_dir().join(&id).join(format!("{id}.json"));
    if let Ok(bytes) = tokio::fs::read(&cache).await
        && let Ok(v) = serde_json::from_slice(&bytes)
    {
        return Ok(Some(v));
    }
    Ok(Some(net::get_json_cached(&url, &cache).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_mod_era() {
        assert!(is_jar_mod_era("1.5.2") && is_jar_mod_era("1.2.5"));
        assert!(!is_jar_mod_era("1.6.4") && !is_jar_mod_era("1.12.2") && !is_jar_mod_era("1.20.1") && !is_jar_mod_era("26.1"));
    }

    #[test]
    fn loader_serde_names() {
        let t = toml::to_string(&crate::instance::Instance::new("x", "1.20.1", Loader::NeoForge("47.1.106".into()))).unwrap();
        assert!(t.contains("kind = \"neoforge\""), "{t}");
        assert_eq!(Loader::from_modrinth("forge", "47.4.10".into()), Some(Loader::Forge("47.4.10".into())));
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn forge_builds_from_prism() {
        let builds = versions_for(LoaderKind::Forge, "1.20.1").await.unwrap();
        assert!(builds.iter().any(|b| b.version == "47.4.10"));
        assert!(builds.iter().any(|b| b.preferred));
        assert!(versions_for(LoaderKind::Forge, "1.5.2").await.is_err());
        let neo = versions_for(LoaderKind::NeoForge, "1.21.1").await.unwrap();
        assert!(neo.iter().any(|b| b.version == "21.1.209"));
    }
}
