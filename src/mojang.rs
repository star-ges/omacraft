//! Mojang's launcher metadata: the version manifest, per-version JSON, and the
//! rule/argument format shared by vanilla and Fabric/Quilt loader profiles.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::net;
use crate::paths;

pub const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
pub const RESOURCES_URL: &str = "https://resources.download.minecraft.net";

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub latest: Latest,
    pub versions: Vec<ManifestVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Latest {
    pub release: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(rename = "releaseTime")]
    pub release_time: chrono::DateTime<chrono::Utc>,
}

pub async fn fetch_manifest() -> anyhow::Result<Manifest> {
    net::get_json_cached(MANIFEST_URL, &paths::meta_cache_dir().join("version_manifest_v2.json")).await
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionJson {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits_from: Option<String>,
    #[serde(default)]
    pub main_class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minecraft_arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Arguments>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_index: Option<AssetIndexRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assets: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<VersionDownloads>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub java_version: Option<JavaVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Prism meta: files to fetch into the libraries dir but leave off the
    /// classpath (the Forge installer and its processors).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maven_files: Vec<Library>,
    /// Prism meta: launchwrapper tweak classes (1.6–1.12 Forge).
    #[serde(default, rename = "+tweakers", skip_serializing_if = "Vec::is_empty")]
    pub tweakers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Arguments {
    #[serde(default)]
    pub game: Vec<Argument>,
    #[serde(default)]
    pub jvm: Vec<Argument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Argument {
    Plain(String),
    Conditional { rules: Vec<Rule>, value: OneOrMany },
}

impl Argument {
    pub fn mentions(&self, token: &str) -> bool {
        match self {
            Argument::Plain(s) => s.contains(token),
            Argument::Conditional { value: OneOrMany::One(s), .. } => s.contains(token),
            Argument::Conditional { value: OneOrMany::Many(v), .. } => v.iter().any(|s| s.contains(token)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndexRef {
    pub id: String,
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDownloads {
    pub client: Artifact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    #[serde(default)]
    pub path: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersion {
    pub component: String,
    pub major_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logging {
    pub client: Option<LoggingClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingClient {
    pub argument: String,
    pub file: LoggingFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingFile {
    pub id: String,
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<LibraryDownloads>,
    /// Maven repository base, used by loader profiles instead of `downloads`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<Rule>>,
    /// Pre-1.19 native libraries: os name -> classifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub natives: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<Extract>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryDownloads {
    pub artifact: Option<Artifact>,
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extract {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<OsRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsRule {
    pub name: Option<String>,
    pub arch: Option<String>,
}

// ---------------------------------------------------------------------------

pub const OS_NAME: &str = if cfg!(target_os = "windows") {
    "windows"
} else if cfg!(target_os = "macos") {
    "osx"
} else {
    "linux"
};

#[derive(Debug, Clone, Default)]
pub struct Features {
    pub custom_resolution: bool,
    pub quick_play_singleplayer: bool,
    pub quick_play_multiplayer: bool,
}

pub fn rules_allow(rules: &[Rule], features: &Features) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        let mut matches = true;
        if let Some(os) = &rule.os {
            if let Some(name) = &os.name
                && name != OS_NAME
            {
                matches = false;
            }
            if let Some(arch) = &os.arch {
                let ours = if cfg!(target_pointer_width = "64") { "x86_64" } else { "x86" };
                let arm = cfg!(target_arch = "aarch64");
                matches &= match arch.as_str() {
                    "x86" => ours == "x86" && !arm,
                    "arm64" | "aarch64" => arm,
                    _ => true,
                };
            }
        }
        if let Some(feats) = &rule.features {
            for (k, want) in feats {
                let have = match k.as_str() {
                    "has_custom_resolution" => features.custom_resolution,
                    "has_quick_plays_support" => features.quick_play_singleplayer || features.quick_play_multiplayer,
                    "is_quick_play_singleplayer" => features.quick_play_singleplayer,
                    "is_quick_play_multiplayer" => features.quick_play_multiplayer,
                    _ => false,
                };
                matches &= have == *want;
            }
        }
        if matches {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

/// `group:artifact:version[:classifier][@ext]` -> relative maven path.
pub fn maven_path(name: &str) -> Option<String> {
    let (coords, ext) = match name.split_once('@') {
        Some((c, e)) => (c, e),
        None => (name, "jar"),
    };
    let parts: Vec<&str> = coords.split(':').collect();
    let (group, artifact, version) = (parts.first()?, parts.get(1)?, parts.get(2)?);
    let file = match parts.get(3) {
        Some(classifier) => format!("{artifact}-{version}-{classifier}.{ext}"),
        None => format!("{artifact}-{version}.{ext}"),
    };
    Some(format!("{}/{artifact}/{version}/{file}", group.replace('.', "/")))
}

/// Identity used when a child profile overrides a parent's library.
fn library_key(name: &str) -> String {
    let parts: Vec<&str> = name.split('@').next().unwrap_or(name).split(':').collect();
    match parts.as_slice() {
        [g, a, _v, c, ..] => format!("{g}:{a}:{c}"),
        [g, a, ..] => format!("{g}:{a}"),
        _ => name.to_string(),
    }
}

impl Library {
    pub fn applies(&self, features: &Features) -> bool {
        self.rules.as_deref().is_none_or(|r| rules_allow(r, features))
    }

    /// The classpath artifact (if any) for this library.
    pub fn artifact(&self) -> Option<net::Download> {
        if let Some(a) = self.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
            let path = a.path.clone().or_else(|| maven_path(&self.name))?;
            if a.url.is_empty() {
                return None;
            }
            return Some(net::Download {
                url: a.url.clone(),
                path: paths::libraries_dir().join(path),
                sha1: a.sha1.clone(),
                size: a.size,
            });
        }
        if self.downloads.is_some() && self.natives.is_some() {
            return None; // natives-only entry
        }
        let rel = maven_path(&self.name)?;
        let base = self.url.as_deref().unwrap_or("https://libraries.minecraft.net/");
        let base = base.trim_end_matches('/');
        Some(net::Download {
            url: format!("{base}/{rel}"),
            path: paths::libraries_dir().join(rel),
            sha1: self.sha1.clone(),
            size: self.size,
        })
    }

    /// Legacy native jar to extract into the natives dir.
    pub fn native(&self) -> Option<net::Download> {
        let classifier = self.natives.as_ref()?.get(OS_NAME)?;
        let arch = if cfg!(target_pointer_width = "64") { "64" } else { "32" };
        let classifier = classifier.replace("${arch}", arch);
        let a = self.downloads.as_ref()?.classifiers.as_ref()?.get(&classifier)?;
        let path = a
            .path
            .clone()
            .or_else(|| maven_path(&format!("{}:{classifier}", self.name)))?;
        Some(net::Download {
            url: a.url.clone(),
            path: paths::libraries_dir().join(path),
            sha1: a.sha1.clone(),
            size: a.size,
        })
    }
}

/// `--flag value` pairs from a legacy argument string whose flag `existing` lacks.
fn extra_args(legacy: &str, existing: &[Argument]) -> Vec<String> {
    let has = |flag: &str| {
        existing.iter().any(|a| match a {
            Argument::Plain(s) => s == flag,
            Argument::Conditional { value: OneOrMany::One(s), .. } => s == flag,
            Argument::Conditional { value: OneOrMany::Many(v), .. } => v.iter().any(|s| s == flag),
        })
    };
    let tokens: Vec<&str> = legacy.split_whitespace().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let flag = tokens[i];
        let value = tokens.get(i + 1).filter(|v| !v.starts_with("--"));
        if !has(flag) {
            out.push(flag.to_string());
            out.extend(value.map(|v| v.to_string()));
        }
        i += 1 + value.is_some() as usize;
    }
    out
}

impl VersionJson {
    /// Layer a loader profile (Fabric/Quilt/Forge/NeoForge) over its parent version.
    pub fn merge_child(mut self, child: VersionJson) -> VersionJson {
        let child_keys: std::collections::HashSet<String> =
            child.libraries.iter().map(|l| library_key(&l.name)).collect();
        let mut libs = child.libraries;
        libs.extend(self.libraries.into_iter().filter(|l| !child_keys.contains(&library_key(&l.name))));
        self.libraries = libs;
        if !child.main_class.is_empty() {
            self.main_class = child.main_class;
        }
        match (child.minecraft_arguments, &mut self.arguments) {
            // Prism's Forge profiles restate the whole legacy argument string;
            // on a modern parent keep vanilla's (with its feature rules) and add
            // only the flags it lacks, like `--launchTarget` and `--fml.*`.
            (Some(legacy), Some(a)) => a.game.extend(extra_args(&legacy, &a.game).into_iter().map(Argument::Plain)),
            (Some(legacy), None) => self.minecraft_arguments = Some(legacy),
            (None, _) => {}
        }
        self.maven_files.extend(child.maven_files);
        self.tweakers.extend(child.tweakers);
        if let Some(ca) = child.arguments {
            let a = self.arguments.get_or_insert_with(Default::default);
            a.game.extend(ca.game);
            a.jvm.extend(ca.jvm);
        }
        self.id = child.id;
        self.inherits_from = None;
        self
    }

    pub fn client_jar(&self) -> PathBuf {
        let id = self.inherits_from.clone().unwrap_or_else(|| self.id.clone());
        paths::versions_dir().join(&id).join(format!("{id}.jar"))
    }

    pub fn java_component(&self) -> String {
        self.java_version
            .as_ref()
            .map(|j| j.component.clone())
            .unwrap_or_else(|| "jre-legacy".into())
    }
}

pub async fn fetch_version(manifest: &Manifest, id: &str) -> anyhow::Result<VersionJson> {
    let path = paths::versions_dir().join(id).join(format!("{id}.json"));
    if let Ok(bytes) = tokio::fs::read(&path).await
        && let Ok(v) = serde_json::from_slice(&bytes)
    {
        return Ok(v);
    }
    let entry = manifest
        .versions
        .iter()
        .find(|v| v.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown Minecraft version {id}"))?;
    net::get_json_cached(&entry.url, &path).await
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndex {
    pub objects: HashMap<String, AssetObject>,
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
    #[serde(default)]
    pub map_to_resources: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetObject {
    #[serde(deserialize_with = "sha1_hex")]
    pub hash: String,
    pub size: u64,
}

/// Asset hashes become paths (`objects/ab/abcd…`), so anything but a SHA-1 is refused.
fn sha1_hex<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let h = String::deserialize(d)?;
    if h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(h)
    } else {
        Err(serde::de::Error::custom(format!("asset hash {h:?} is not a SHA-1")))
    }
}

impl AssetObject {
    pub fn rel(&self) -> String {
        format!("{}/{}", &self.hash[..2], self.hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_hashes_must_be_sha1() {
        let ok: AssetIndex = serde_json::from_str(r#"{"objects":{"a.png":{"hash":"bdf48ef6b5d0d23bbb02e17d04865216179f510a","size":1}}}"#).unwrap();
        assert_eq!(ok.objects["a.png"].rel(), "bd/bdf48ef6b5d0d23bbb02e17d04865216179f510a");
        for bad in ["a", "", "../../../../etc/passwd/0000000000000000000000", "zzf48ef6b5d0d23bbb02e17d04865216179f510a", "éé48ef6b5d0d23bbb02e17d04865216179f510"] {
            let json = format!(r#"{{"objects":{{"a.png":{{"hash":{bad:?},"size":1}}}}}}"#);
            assert!(serde_json::from_str::<AssetIndex>(&json).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn forge_args_layer_onto_modern_vanilla() {
        let vanilla: VersionJson = serde_json::from_str(
            r#"{"id":"1.20.1","mainClass":"net.minecraft.client.main.Main","arguments":{"game":["--username","${auth_player_name}","--version","${version_name}",
            {"rules":[{"action":"allow","features":{"has_custom_resolution":true}}],"value":["--width","${resolution_width}"]}],"jvm":["-cp","${classpath}"]}}"#,
        )
        .unwrap();
        let forge: VersionJson = serde_json::from_str(
            r#"{"mainClass":"io.github.zekerzhayard.forgewrapper.installer.Main","minecraftArguments":"--username ${auth_player_name} --version ${version_name} --width 1 --launchTarget forgeclient --fml.forgeVersion 47.4.10 --demo",
            "libraries":[{"name":"io.github.zekerzhayard:ForgeWrapper:prism","downloads":{"artifact":{"url":"https://example.com/fw.jar"}}}],
            "mavenFiles":[{"name":"net.minecraftforge:forge:1.20.1-47.4.10:installer","downloads":{"artifact":{"url":"https://example.com/i.jar"}}}]}"#,
        )
        .unwrap();
        let merged = vanilla.merge_child(forge);
        let game: Vec<String> = merged.arguments.unwrap().game.iter().filter_map(|a| match a {
            Argument::Plain(s) => Some(s.clone()),
            _ => None,
        }).collect();
        assert_eq!(game, ["--username", "${auth_player_name}", "--version", "${version_name}", "--launchTarget", "forgeclient", "--fml.forgeVersion", "47.4.10", "--demo"]);
        assert!(merged.minecraft_arguments.is_none());
        assert_eq!(merged.main_class, crate::loaders::FORGE_WRAPPER_MAIN);
        assert_eq!(merged.maven_files.len(), 1);
        assert_eq!(merged.maven_files[0].artifact().unwrap().path, paths::libraries_dir().join("net/minecraftforge/forge/1.20.1-47.4.10/forge-1.20.1-47.4.10-installer.jar"));
    }

    #[test]
    fn legacy_forge_keeps_vanilla_args_and_tweakers() {
        let vanilla: VersionJson = serde_json::from_str(r#"{"id":"1.12.2","mainClass":"net.minecraft.client.main.Main","minecraftArguments":"--username ${auth_player_name}"}"#).unwrap();
        let forge: VersionJson = serde_json::from_str(r#"{"+tweakers":["net.minecraftforge.fml.common.launcher.FMLTweaker"],"mainClass":"net.minecraft.launchwrapper.Launch","libraries":[{"name":"net.minecraft:launchwrapper:1.12"}]}"#).unwrap();
        let merged = vanilla.merge_child(forge);
        assert_eq!(merged.minecraft_arguments.as_deref(), Some("--username ${auth_player_name}"));
        assert_eq!(merged.tweakers, ["net.minecraftforge.fml.common.launcher.FMLTweaker"]);
        assert_eq!(merged.main_class, "net.minecraft.launchwrapper.Launch");
        assert_eq!(merged.libraries[0].artifact().unwrap().url, "https://libraries.minecraft.net/net/minecraft/launchwrapper/1.12/launchwrapper-1.12.jar");
    }
}
