//! Mod sets: named on/off arrangements of one instance's mods ("performance only",
//! "full pack"), switched by the usual `.disabled` renames instead of duplicating the
//! instance. Kept in `mod_sets.toml` in the instance folder, beside (not inside)
//! `instance.toml`, which other dialogs rewrite whole.
//!
//! - A set names mods by [`key`]: the id the jar gives itself, else its file name
//!   without the version, so an update from `a-1.jar` to `a-2.jar` stays in its sets.
//! - Each set remembers the mods it has on and the ones it keeps off. A mod a set has
//!   never seen is off when switching to it, and the mods tab offers to add it.
//! - While a set is active, a newly enabled mod it hasn't seen joins it ([`ModSets::absorb`]);
//!   switching a known mod on or off marks the set modified, to update or revert.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::instance::{Instance, ModFile};
use crate::{content_meta, fsutil};

pub const FILE: &str = "mod_sets.toml";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModSets {
    /// The set the mods were last switched to or saved as; empty for none.
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub sets: Vec<ModSet>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModSet {
    pub name: String,
    /// Mods it has on, by [`key`].
    #[serde(default)]
    pub on: BTreeSet<String>,
    /// Mods it has seen and keeps off.
    #[serde(default)]
    pub off: BTreeSet<String>,
}

/// One installed mod file and its [`key`].
#[derive(Debug, Clone)]
pub struct Entry {
    pub file: ModFile,
    pub key: String,
}

/// What switching to a set would do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub enable: Vec<PathBuf>,
    pub disable: Vec<PathBuf>,
    /// Installed mods the set has never seen (they end up off).
    pub unseen: Vec<String>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty() && self.disable.is_empty()
    }

    /// "enables 3, disables 12", or "changes nothing".
    pub fn summary(&self) -> String {
        match (self.enable.len(), self.disable.len()) {
            (0, 0) => "changes nothing".into(),
            (e, 0) => format!("enables {e}"),
            (0, d) => format!("disables {d}"),
            (e, d) => format!("enables {e}, disables {d}"),
        }
    }
}

/// How a mod is named in sets: its own id, else its file name without the version
/// (`file:sodium-fabric` for `sodium-fabric-0.5.8+mc1.20.1.jar`).
pub fn key(mod_id: Option<&str>, file_name: &str) -> String {
    match mod_id.filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => format!("file:{}", stem(file_name)),
    }
}

/// A jar's file name without `.disabled`, `.jar` and the version: the tokens before
/// the first that starts with a digit (or `v`/`mc` and a digit).
fn stem(file_name: &str) -> String {
    let base = file_name.trim_end_matches(".disabled").trim_end_matches(".jar").to_lowercase();
    let version = |t: &str| {
        let t = t.strip_prefix("mc").or_else(|| t.strip_prefix('v')).unwrap_or(t);
        t.starts_with(|c: char| c.is_ascii_digit())
    };
    let words: Vec<&str> = base.split(['-', '_', '+', ' ']).filter(|t| !t.is_empty()).take_while(|t| !version(t)).collect();
    if words.is_empty() { base } else { words.join("-") }
}

/// The instance's mods with their keys. Reads each jar's metadata: run it off the UI thread.
pub fn scan(inst: &Instance) -> Vec<Entry> {
    inst.mods()
        .into_iter()
        .filter(|f| !f.is_dir)
        .map(|f| {
            let id = content_meta::mod_id(&f.path);
            Entry { key: key(id.as_deref(), &f.name), file: f }
        })
        .collect()
}

/// Keys with at least one enabled file, and keys with none.
pub fn state(entries: &[Entry]) -> (BTreeSet<String>, BTreeSet<String>) {
    let on: BTreeSet<String> = entries.iter().filter(|e| e.file.enabled).map(|e| e.key.clone()).collect();
    let off = entries.iter().filter(|e| !on.contains(&e.key)).map(|e| e.key.clone()).collect();
    (on, off)
}

impl ModSet {
    /// A set of the mods as they are now.
    pub fn of(name: &str, entries: &[Entry]) -> Self {
        let (on, off) = state(entries);
        ModSet { name: name.to_string(), on, off }
    }

    pub fn seen(&self, key: &str) -> bool {
        self.on.contains(key) || self.off.contains(key)
    }

    /// Take the mods as they are now, keeping what it says about mods not installed
    /// (they may come back).
    pub fn update(&mut self, entries: &[Entry]) {
        let (on, off) = state(entries);
        let installed: BTreeSet<&String> = on.iter().chain(&off).collect();
        self.on.retain(|k| !installed.contains(k));
        self.off.retain(|k| !installed.contains(k));
        self.on.extend(on);
        self.off.extend(off);
    }

    /// The renames that would make the installed mods match this set. A mod the set has
    /// on stays as it is if any of its files is enabled, else its newest file is enabled;
    /// one it has off, or has never seen, has every file disabled.
    pub fn change(&self, entries: &[Entry]) -> Change {
        let mut by_key: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        for e in entries {
            by_key.entry(&e.key).or_default().push(e);
        }
        let mut c = Change::default();
        for (k, files) in by_key {
            if self.on.contains(k) {
                if !files.iter().any(|e| e.file.enabled) {
                    let newest = files.iter().max_by(|a, b| a.file.modified.cmp(&b.file.modified).then_with(|| b.file.name.cmp(&a.file.name)));
                    c.enable.extend(newest.map(|e| e.file.path.clone()));
                }
            } else {
                c.disable.extend(files.iter().filter(|e| e.file.enabled).map(|e| e.file.path.clone()));
                if !self.off.contains(k) {
                    c.unseen.push(k.to_string());
                }
            }
        }
        c
    }

    /// Installed mods it has never seen.
    pub fn unseen(&self, entries: &[Entry]) -> Vec<String> {
        let keys: BTreeSet<&String> = entries.iter().map(|e| &e.key).collect();
        keys.into_iter().filter(|k| !self.seen(k)).cloned().collect()
    }
}

impl ModSets {
    pub fn get(&self, name: &str) -> Option<&ModSet> {
        self.sets.iter().find(|s| s.name == name)
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut ModSet> {
        self.sets.iter_mut().find(|s| s.name == name)
    }

    pub fn active(&self) -> Option<&ModSet> {
        self.get(&self.active)
    }

    /// Whether the mods differ from the active set (always false with none active).
    pub fn modified(&self, entries: &[Entry]) -> bool {
        self.active().is_some_and(|s| !s.change(entries).is_empty())
    }

    /// Mods switched on that the active set has never seen join it: a mod installed
    /// while a set is active belongs to it. Returns whether anything changed.
    pub fn absorb(&mut self, entries: &[Entry]) -> bool {
        let active = self.active.clone();
        let Some(set) = self.get_mut(&active) else { return false };
        let new: Vec<String> = entries.iter().filter(|e| e.file.enabled && !set.seen(&e.key)).map(|e| e.key.clone()).collect();
        let changed = !new.is_empty();
        set.on.extend(new);
        changed
    }

    /// Save the mods as they are now as a new set, and make it the active one.
    pub fn add(&mut self, name: &str, entries: &[Entry]) -> anyhow::Result<()> {
        let name = check_name(name)?;
        if self.get(&name).is_some() {
            bail!("there's already a set called “{name}”");
        }
        self.sets.push(ModSet::of(&name, entries));
        self.active = name;
        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
        let to = check_name(to)?;
        if to != from && self.get(&to).is_some() {
            bail!("there's already a set called “{to}”");
        }
        let set = self.get_mut(from).with_context(|| format!("no set called “{from}”"))?;
        set.name = to.clone();
        if self.active == from {
            self.active = to;
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) {
        self.sets.retain(|s| s.name != name);
        if self.active == name {
            self.active.clear();
        }
    }

    /// Update set `name` to the mods as they are now (and make it active).
    pub fn update(&mut self, name: &str, entries: &[Entry]) -> anyhow::Result<()> {
        self.get_mut(name).with_context(|| format!("no set called “{name}”"))?.update(entries);
        self.active = name.to_string();
        Ok(())
    }

    /// Note that the active set keeps these mods (which it hadn't seen) off.
    pub fn keep_off(&mut self, keys: &[String]) {
        let active = self.active.clone();
        if let Some(set) = self.get_mut(&active) {
            set.off.extend(keys.iter().filter(|k| !set.on.contains(*k)).cloned());
        }
    }
}

fn check_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("give the set a name");
    }
    if name.chars().count() > 64 || name.chars().any(char::is_control) {
        bail!("that name won't do: keep it under 64 characters, on one line");
    }
    Ok(name.to_string())
}

pub fn path(inst: &Instance) -> PathBuf {
    inst.dir().join(FILE)
}

/// The instance's sets; none if it has no file yet.
pub fn load(inst: &Instance) -> anyhow::Result<ModSets> {
    load_from(&path(inst))
}

fn load_from(p: &Path) -> anyhow::Result<ModSets> {
    match std::fs::read(p) {
        Ok(bytes) => toml::from_str(&String::from_utf8_lossy(&bytes)).with_context(|| format!("reading {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ModSets::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save(inst: &Instance, sets: &ModSets) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(sets)?;
    fsutil::write_atomic(&path(inst), text.as_bytes(), 0o644)?;
    Ok(())
}

/// Enable and disable files as `c` says. Stops at the first file it can't rename
/// (say both `a.jar` and `a.jar.disabled` exist); what it did so far stays done.
pub fn apply(c: &Change) -> anyhow::Result<()> {
    for p in &c.disable {
        rename(p, false)?;
    }
    for p in &c.enable {
        rename(p, true)?;
    }
    Ok(())
}

/// Turn the mod file at `p` on or off by renaming it; nothing if it already is.
pub fn rename(p: &Path, on: bool) -> anyhow::Result<()> {
    let name = p.file_name().context("no file name")?.to_string_lossy().into_owned();
    let is_on = !name.ends_with(".disabled");
    if is_on == on {
        return Ok(());
    }
    let to = if on { p.with_file_name(name.trim_end_matches(".disabled")) } else { p.with_file_name(format!("{name}.disabled")) };
    if std::fs::symlink_metadata(&to).is_ok() {
        bail!("{} already exists; remove one of the two first", to.file_name().unwrap_or_default().to_string_lossy());
    }
    std::fs::rename(p, &to).with_context(|| format!("renaming {name}"))
}

/// Switch `inst`'s mods to set `name`: rename the files, make it the active set.
/// Returns what it did.
pub fn switch(inst: &Instance, name: &str) -> anyhow::Result<Change> {
    let mut sets = load(inst)?;
    let set = sets.get(name).with_context(|| format!("no set called “{name}”"))?.clone();
    let entries = scan(inst);
    let c = set.change(&entries);
    apply(&c)?;
    sets.active = name.to_string();
    save(inst, &sets)?;
    Ok(c)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::loaders::Loader;

    /// A Fabric mod jar `name` in `inst`'s mods folder, with id `id` (none: no metadata).
    pub fn jar(inst: &Instance, name: &str, id: Option<&str>, depends: &[&str]) -> PathBuf {
        use std::io::Write;
        let dir = inst.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        if let Some(id) = id {
            z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap();
            let deps: Vec<String> = std::iter::once(&"minecraft").chain(depends).map(|d| format!("\"{d}\": \"*\"")).collect();
            write!(z, r#"{{"schemaVersion": 1, "id": "{id}", "version": "1.0", "depends": {{{}}}}}"#, deps.join(", ")).unwrap();
        } else {
            z.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
        }
        z.finish().unwrap();
        path
    }

    fn inst(name: &str) -> Instance {
        let mut i = Instance::new(name, "1.21.1", Loader::Fabric("0.16.5".into()));
        i.assign_id().unwrap();
        i
    }

    fn on_names(inst: &Instance) -> Vec<String> {
        inst.mods().into_iter().filter(|m| m.enabled).map(|m| m.name).collect()
    }

    #[test]
    fn keys_ignore_versions() {
        assert_eq!(key(Some("sodium"), "whatever.jar"), "sodium");
        assert_eq!(key(None, "sodium-fabric-0.5.8+mc1.20.1.jar"), "file:sodium-fabric");
        assert_eq!(key(None, "Sodium-Fabric-0.6.0.jar.disabled"), "file:sodium-fabric");
        assert_eq!(key(None, "worldedit-mod-v7.3.jar"), "file:worldedit-mod");
        assert_eq!(key(Some(""), "1.2.3.jar"), "file:1.2.3");
    }

    #[test]
    fn switching_sets_is_exact() {
        let inst = inst("sets switch");
        jar(&inst, "sodium-0.6.jar", Some("sodium"), &[]);
        jar(&inst, "iris-1.8.jar", Some("iris"), &[]);
        jar(&inst, "create-6.0.jar", Some("create"), &[]);
        jar(&inst, "plain-2.1.jar", None, &[]);
        let mut sets = ModSets::default();
        sets.add("full", &scan(&inst)).unwrap();

        // "perf": only sodium and iris.
        for m in inst.mods().iter().filter(|m| ["create-6.0.jar", "plain-2.1.jar"].contains(&m.name.as_str())) {
            m.toggle().unwrap();
        }
        assert!(sets.modified(&scan(&inst)));
        sets.add("perf", &scan(&inst)).unwrap();
        assert_eq!(sets.active, "perf");
        save(&inst, &sets).unwrap();
        assert_eq!(load(&inst).unwrap(), sets);

        let c = switch(&inst, "full").unwrap();
        assert_eq!(c.summary(), "enables 2");
        assert_eq!(on_names(&inst), ["create-6.0.jar", "iris-1.8.jar", "plain-2.1.jar", "sodium-0.6.jar"]);
        assert!(!load(&inst).unwrap().modified(&scan(&inst)));
        let c = switch(&inst, "perf").unwrap();
        assert_eq!(c.summary(), "disables 2");
        assert_eq!(on_names(&inst), ["iris-1.8.jar", "sodium-0.6.jar"]);
        assert!(switch(&inst, "perf").unwrap().is_empty(), "switching to the active set changes nothing");
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn membership_survives_an_update_rename() {
        let inst = inst("sets update");
        let old = jar(&inst, "a-1.0.jar", Some("a"), &[]);
        let plain = jar(&inst, "plain-1.0.jar", None, &[]);
        let mut sets = ModSets::default();
        sets.add("with a", &scan(&inst)).unwrap();
        mod_off(&old);
        mod_off(&plain);
        sets.add("without a", &scan(&inst)).unwrap();

        // Both updated, as `modrinth::update` does: the new file in, the old one out.
        std::fs::remove_file(old.with_file_name("a-1.0.jar.disabled")).unwrap();
        std::fs::remove_file(plain.with_file_name("plain-1.0.jar.disabled")).unwrap();
        jar(&inst, "a-2.0.jar.disabled", Some("a"), &[]);
        jar(&inst, "plain-1.1.jar.disabled", None, &[]);
        let entries = scan(&inst);
        assert!(!sets.modified(&entries), "still the set's own mods");
        assert!(sets.get("with a").unwrap().unseen(&entries).is_empty());
        let c = sets.get("with a").unwrap().change(&entries);
        assert_eq!(c.enable.len(), 2);
        apply(&c).unwrap();
        assert_eq!(on_names(&inst), ["a-2.0.jar", "plain-1.1.jar"]);
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    fn mod_off(p: &Path) {
        rename(p, false).unwrap();
    }

    #[test]
    fn new_mods_join_the_active_set_and_stay_off_elsewhere() {
        let inst = inst("sets new");
        jar(&inst, "a-1.jar", Some("a"), &[]);
        let mut sets = ModSets::default();
        sets.add("one", &scan(&inst)).unwrap();
        sets.add("two", &scan(&inst)).unwrap();
        // Installed while "two" is active: it joins "two" only.
        jar(&inst, "b-1.jar", Some("b"), &[]);
        let entries = scan(&inst);
        assert!(sets.absorb(&entries));
        assert!(!sets.absorb(&entries), "once");
        assert!(!sets.modified(&entries));
        assert!(sets.get("two").unwrap().on.contains("b"));
        let one = sets.get("one").unwrap();
        assert_eq!(one.unseen(&entries), ["b"]);
        let c = one.change(&entries);
        assert_eq!((c.disable.len(), c.unseen.clone()), (1, vec!["b".to_string()]));

        // Switched to "one", which then keeps it off once told so.
        sets.active = "one".into();
        apply(&c).unwrap();
        let entries = scan(&inst);
        assert!(!sets.absorb(&entries), "an unseen mod that's off doesn't join");
        sets.keep_off(&["b".into()]);
        assert!(sets.get("one").unwrap().unseen(&entries).is_empty());
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn duplicates_of_one_mod_keep_the_copy_in_use() {
        let inst = inst("sets dupes");
        jar(&inst, "a-1.jar", Some("a"), &[]);
        let mut sets = ModSets::default();
        sets.add("on", &scan(&inst)).unwrap();
        // A second copy, off: the set leaves the enabled one alone.
        jar(&inst, "a-2.jar.disabled", Some("a"), &[]);
        assert!(sets.get("on").unwrap().change(&scan(&inst)).is_empty());
        // With both off, the newest comes on.
        mod_off(&inst.game_dir().join("mods/a-1.jar"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = inst.game_dir().join("mods/a-2.jar.disabled");
        std::fs::write(&newer, std::fs::read(&newer).unwrap()).unwrap();
        let c = sets.get("on").unwrap().change(&scan(&inst));
        assert_eq!(c.enable, [newer]);
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn names_and_updates() {
        let inst = inst("sets names");
        jar(&inst, "a-1.jar", Some("a"), &[]);
        let mut sets = ModSets::default();
        assert!(sets.add("  ", &scan(&inst)).is_err());
        sets.add("main", &scan(&inst)).unwrap();
        assert!(sets.add("main", &scan(&inst)).is_err());
        sets.add("other", &scan(&inst)).unwrap();
        assert!(sets.rename("other", "main").is_err());
        sets.rename("other", "lite").unwrap();
        assert_eq!(sets.active, "lite");
        // An update forgets nothing about mods that aren't installed now.
        sets.get_mut("lite").unwrap().on.insert("gone".into());
        mod_off(&inst.game_dir().join("mods/a-1.jar"));
        sets.update("lite", &scan(&inst)).unwrap();
        let lite = sets.get("lite").unwrap();
        assert!(lite.on.contains("gone") && lite.off.contains("a") && !lite.on.contains("a"));
        sets.remove("lite");
        assert!(sets.active.is_empty() && sets.get("lite").is_none());
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }
}
