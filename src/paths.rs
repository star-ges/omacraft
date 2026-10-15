//! Filesystem layout. Shared caches (libraries, assets, runtimes) live once in
//! the data dir; each instance gets its own game directory, like Prism.

use std::path::PathBuf;

pub const APP_ID: &str = "omacraft";

#[cfg(not(test))]
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP_ID)
}

#[cfg(not(test))]
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP_ID)
}

/// Tests never touch the real data: everything goes under a scratch root per test run.
#[cfg(test)]
pub fn test_root() -> PathBuf {
    std::env::temp_dir().join(format!("omacraft-test-{}", std::process::id()))
}

#[cfg(test)]
pub fn data_dir() -> PathBuf {
    test_root().join("data").join(APP_ID)
}

#[cfg(test)]
pub fn config_dir() -> PathBuf {
    test_root().join("config").join(APP_ID)
}

pub fn instances_dir() -> PathBuf {
    data_dir().join("instances")
}

pub fn libraries_dir() -> PathBuf {
    data_dir().join("libraries")
}

pub fn assets_dir() -> PathBuf {
    data_dir().join("assets")
}

pub fn versions_dir() -> PathBuf {
    data_dir().join("versions")
}

pub fn runtimes_dir() -> PathBuf {
    data_dir().join("runtimes")
}

pub fn meta_cache_dir() -> PathBuf {
    data_dir().join("meta")
}

pub fn accounts_file() -> PathBuf {
    data_dir().join("accounts.json")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Where `omarchy theme set` atomically swaps in the active theme.
pub fn omarchy_theme_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local/state/omarchy/current/theme")
}
