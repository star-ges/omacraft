//! Instance groups: order, colour, and groups with no instances yet. Each
//! instance names its group in instance.toml; this file adds the rest
//! (Prism keeps the same kind of thing in instgroups.json).

use serde::{Deserialize, Serialize};

use crate::instance::Instance;
use crate::paths;

/// Palette names a group can be tinted with. Stored by name so the colour
/// follows the Omarchy theme.
pub const COLORS: [&str; 7] = ["red", "orange", "yellow", "green", "cyan", "blue", "magenta"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Group {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Groups {
    #[serde(default, rename = "group")]
    pub list: Vec<Group>,
}

fn file() -> std::path::PathBuf {
    paths::data_dir().join("groups.toml")
}

impl Groups {
    pub fn load() -> Self {
        std::fs::read_to_string(file()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        crate::fsutil::write_atomic(&file(), text.as_bytes(), 0o644)?;
        Ok(())
    }

    /// Add any group an instance names but we don't know yet. Returns true
    /// if anything was added.
    pub fn sync(&mut self, instances: &[Instance]) -> bool {
        let mut missing: Vec<String> = instances
            .iter()
            .map(|i| i.group.clone())
            .filter(|g| !g.is_empty() && !self.list.iter().any(|x| x.name == *g))
            .collect();
        missing.sort_by_key(|g| g.to_lowercase());
        missing.dedup();
        let added = !missing.is_empty();
        self.list.extend(missing.into_iter().map(|name| Group { name, color: None }));
        added
    }

    pub fn names(&self) -> Vec<String> {
        self.list.iter().map(|g| g.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Group> {
        self.list.iter().find(|g| g.name == name)
    }

    pub fn color(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|g| g.color.as_deref())
    }
}
