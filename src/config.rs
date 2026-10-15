use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::paths;

/// Omacraft's Azure app registration, approved by Mojang for the Minecraft API.
pub const DEFAULT_CLIENT_ID: &str = "5f3b9b5e-4f2d-4fa7-a1e2-cbb4260041ed";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Override for the Azure app (client) ID used for Microsoft sign-in.
    /// Empty means Omacraft's own ID. Can also be set with OMACRAFT_MSA_CLIENT_ID.
    pub msa_client_id: String,
    /// Default max heap for new instances, in MiB.
    pub default_memory_mb: u32,
    /// Use this java binary instead of Mojang's bundled runtime.
    pub java_override: String,
    /// Extra JVM flags applied to every instance.
    pub global_jvm_args: String,
    pub show_snapshots: bool,
    /// Instance groups folded shut in the list.
    pub collapsed_groups: Vec<String>,
    /// Rebound keys: action id → chords (see keymap.rs).
    pub keys: BTreeMap<String, Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            msa_client_id: String::new(),
            default_memory_mb: 4096,
            java_override: String::new(),
            global_jvm_args: String::new(),
            show_snapshots: false,
            collapsed_groups: Vec::new(),
            keys: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(paths::config_file())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        crate::fsutil::write_atomic(&paths::config_file(), text.as_bytes(), 0o644)?;
        Ok(())
    }

    pub fn client_id(&self) -> String {
        std::env::var("OMACRAFT_MSA_CLIENT_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| Some(self.msa_client_id.trim().to_string()).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
    }
}
