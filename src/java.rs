//! Java runtimes. Like Prism's auto-install, we fetch the exact runtime Mojang
//! ships for each Minecraft version (java-runtime-delta, jre-legacy, ...).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::net::{self, Download};
use crate::paths;
use crate::task::Task;

const RUNTIMES_URL: &str =
    "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";

/// Mojang's name for x86_64 Linux, the only platform games launch on (`launch::check_platform`).
const PLATFORM: &str = "linux";

#[derive(Deserialize)]
struct RuntimeEntry {
    manifest: ManifestRef,
    version: RuntimeVersion,
}

#[derive(Deserialize)]
struct ManifestRef {
    url: String,
}

#[derive(Deserialize)]
struct RuntimeVersion {
    name: String,
}

#[derive(Deserialize)]
struct RuntimeManifest {
    files: HashMap<String, RuntimeFile>,
}

#[derive(Deserialize)]
struct RuntimeFile {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    executable: bool,
    downloads: Option<RuntimeDownloads>,
    target: Option<String>,
}

#[derive(Deserialize)]
struct RuntimeDownloads {
    raw: crate::mojang::Artifact,
}

pub fn runtime_dir(component: &str) -> PathBuf {
    paths::runtimes_dir().join(component)
}

pub fn java_binary(component: &str) -> PathBuf {
    runtime_dir(component).join("bin/java")
}

/// Make sure Mojang's runtime `component` is installed; returns its java.
pub async fn ensure_runtime(task: &Task, component: &str) -> anyhow::Result<PathBuf> {
    let dir = runtime_dir(component);
    let marker = dir.join(".omacraft-version");
    let java = java_binary(component);

    let all: HashMap<String, HashMap<String, Vec<RuntimeEntry>>> =
        match net::get_json_cached(RUNTIMES_URL, &paths::meta_cache_dir().join("java-runtimes.json")).await {
            Ok(a) => a,
            Err(e) if java.exists() => {
                eprintln!("java runtime index unavailable, using installed {component}: {e:#}");
                return Ok(java);
            }
            Err(e) => return Err(e),
        };
    let entry = all
        .get(PLATFORM)
        .and_then(|p| p.get(component))
        .and_then(|v| v.first())
        .with_context(|| format!("Mojang has no {component} runtime for {PLATFORM}; set a Java override in settings"))?;

    if java.exists() && std::fs::read_to_string(&marker).ok().as_deref() == Some(entry.version.name.as_str()) {
        return Ok(java);
    }

    task.stage(format!("Java {} ({component})", entry.version.name), 0);
    let manifest: RuntimeManifest = net::get_json(&entry.manifest.url).await?;
    std::fs::create_dir_all(&dir)?;

    let mut downloads = Vec::new();
    let mut executables = Vec::new();
    let mut links = Vec::new();
    for (rel, f) in &manifest.files {
        let path = dir.join(rel);
        match f.kind.as_str() {
            "directory" => std::fs::create_dir_all(&path)?,
            "file" => {
                let Some(d) = &f.downloads else { continue };
                downloads.push(Download {
                    url: d.raw.url.clone(),
                    path: path.clone(),
                    sha1: d.raw.sha1.clone(),
                    size: d.raw.size,
                });
                if f.executable {
                    executables.push(path);
                }
            }
            "link" => {
                if let Some(t) = &f.target {
                    links.push((path, t.clone()));
                }
            }
            _ => {}
        }
    }
    net::download_all(task, &format!("Java {}", entry.version.name), downloads, 16).await?;

    use std::os::unix::fs::PermissionsExt;
    for p in executables {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    }
    for (path, target) in links {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&path);
        std::os::unix::fs::symlink(target, &path)?;
    }
    if !java.exists() {
        bail!("runtime {component} installed but {} is missing", java.display());
    }
    std::fs::write(&marker, &entry.version.name)?;
    Ok(java)
}
