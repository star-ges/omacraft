//! Instance icons: Prism's built-in set plus the Omacraft logo, and images
//! the user adds (kept in the data dir, referenced as `custom:<file>`).

use std::path::{Path, PathBuf};

use anyhow::bail;
use egui::ImageSource;
use egui::load::Bytes;

use crate::paths;

pub const DEFAULT: &str = "omacraft";

macro_rules! builtin {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../assets/icons/", $name, ".svg")) as &[u8])),*]
    };
}

/// Ordered for the picker: logo, blocks, items, mobs, platforms.
pub const BUILTIN: &[(&str, &[u8])] = builtin!(
    "omacraft", "grass", "dirt", "stone", "planks", "brick", "iron", "gold", "diamond", "tnt", "netherstar", "enderpearl",
    "meat", "flame", "gear", "magitech", "creeper", "squarecreeper", "skeleton", "enderman", "chicken", "bee", "fox",
    "steve", "herobrine", "fabricmc", "quiltmc", "neoforged", "modrinth",
);

pub fn dir() -> PathBuf {
    paths::data_dir().join("icons")
}

pub fn is_builtin(key: &str) -> bool {
    BUILTIN.iter().any(|(n, _)| *n == key)
}

pub fn source(key: &str) -> ImageSource<'static> {
    if let Some(file) = key.strip_prefix("custom:") {
        let p = dir().join(file);
        if p.exists() {
            return ImageSource::Uri(format!("file://{}", p.display()).into());
        }
    }
    let (name, bytes) = BUILTIN.iter().find(|(n, _)| *n == key).unwrap_or(&BUILTIN[0]);
    ImageSource::Bytes { uri: format!("bytes://icon/{name}.svg").into(), bytes: Bytes::Static(bytes) }
}

/// Keys of the user's own icons.
pub fn custom() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_image(Path::new(n)))
        .map(|n| format!("custom:{n}"))
        .collect();
    out.sort();
    out
}

fn is_image(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "svg")
    )
}

/// Copy an image into the icons dir; returns its key.
pub fn add_custom(path: &Path) -> anyhow::Result<String> {
    if !is_image(path) {
        bail!("icons must be PNG, JPEG or SVG");
    }
    std::fs::create_dir_all(dir())?;
    let stem: String = path
        .file_stem()
        .map(|s| s.to_string_lossy().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect())
        .unwrap_or_else(|| "icon".into());
    let ext = path.extension().unwrap().to_string_lossy().to_ascii_lowercase();
    let mut name = format!("{stem}.{ext}");
    let mut n = 2;
    while dir().join(&name).exists() {
        name = format!("{stem}-{n}.{ext}");
        n += 1;
    }
    std::fs::copy(path, dir().join(&name))?;
    Ok(format!("custom:{name}"))
}

pub fn remove_custom(key: &str) {
    if let Some(file) = key.strip_prefix("custom:") {
        let _ = std::fs::remove_file(dir().join(file));
    }
}
