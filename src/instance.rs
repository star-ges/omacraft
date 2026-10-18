use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::loaders::Loader;
use crate::{fsutil, paths};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    #[serde(skip)]
    pub id: String,
    pub name: String,
    pub mc_version: String,
    #[serde(default)]
    pub loader: Loader,
    #[serde(default)]
    pub group: String,
    /// Share set whose resource packs, shader packs and schematics this instance uses (see shares.rs).
    #[serde(default)]
    pub share_set: String,
    /// World set whose `saves` this instance uses (see shares.rs, world_sets.rs).
    #[serde(default)]
    pub world_set: String,
    /// Built-in icon name, or `custom:<file>` under the icons dir.
    #[serde(default = "default_icon")]
    pub icon: String,
    #[serde(default)]
    pub notes: String,
    /// Max heap in MiB; falls back to the global default.
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub jvm_args: String,
    #[serde(default)]
    pub java_path: String,
    #[serde(default)]
    pub window: Option<(u32, u32)>,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub last_played: Option<DateTime<Utc>>,
    #[serde(default)]
    pub played_secs: u64,
}

impl Instance {
    pub fn new(name: &str, mc_version: &str, loader: Loader) -> Self {
        Self {
            id: String::new(),
            name: name.to_string(),
            mc_version: mc_version.to_string(),
            loader,
            group: String::new(),
            share_set: String::new(),
            world_set: String::new(),
            icon: default_icon(),
            notes: String::new(),
            memory_mb: None,
            jvm_args: String::new(),
            java_path: String::new(),
            window: None,
            created: Utc::now(),
            last_played: None,
            played_secs: 0,
        }
    }

    pub fn dir(&self) -> PathBuf {
        paths::instances_dir().join(&self.id)
    }

    /// The `.minecraft` the game sees.
    pub fn game_dir(&self) -> PathBuf {
        self.dir().join("minecraft")
    }

    pub fn saves_dir(&self) -> PathBuf {
        self.game_dir().join("saves")
    }

    pub fn screenshots_dir(&self) -> PathBuf {
        self.game_dir().join("screenshots")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.game_dir().join("logs")
    }

    pub fn crash_reports_dir(&self) -> PathBuf {
        self.game_dir().join("crash-reports")
    }

    pub fn natives_dir(&self) -> PathBuf {
        self.dir().join("natives")
    }

    /// Whether this instance has game settings to copy into a new one.
    pub fn has_settings(&self) -> bool {
        self.game_dir().join("options.txt").is_file()
    }

    /// Copy another instance's game settings (keybinds, video, sound…) into this one,
    /// once. Never shared live: older versions drop keys they don't know when they save,
    /// and mod keybinds differ between instances. Returns how many files came across.
    pub fn copy_settings_from(&self, from: &Instance) -> std::io::Result<usize> {
        std::fs::create_dir_all(self.game_dir())?;
        let mut n = 0;
        for f in SETTINGS_FILES {
            let src = from.game_dir().join(f);
            if src.is_file() {
                std::fs::copy(&src, self.game_dir().join(f))?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::create_dir_all(self.game_dir())?;
        fsutil::write_atomic(&self.dir().join("instance.toml"), text.as_bytes(), 0o644)?;
        Ok(())
    }

    fn load(dir: &Path) -> anyhow::Result<Self> {
        let mut i: Instance = toml::from_str(&std::fs::read_to_string(dir.join("instance.toml"))?)?;
        i.id = dir.file_name().unwrap().to_string_lossy().into_owned();
        Ok(i)
    }

    /// Pick a free directory name derived from the instance name, and claim it
    /// by creating the directory, so two imports can't end up sharing one.
    pub fn assign_id(&mut self) -> std::io::Result<()> {
        let mut slug: String = self
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        slug = slug.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
        if slug.is_empty() {
            slug = "instance".into();
        }
        std::fs::create_dir_all(paths::instances_dir())?;
        let mut id = slug.clone();
        let mut n = 2;
        loop {
            match std::fs::create_dir(paths::instances_dir().join(&id)) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    id = format!("{slug}-{n}");
                    n += 1;
                }
                Err(e) => return Err(e),
            }
        }
        self.id = id;
        Ok(())
    }

    pub fn mods(&self) -> Vec<ModFile> {
        self.content(ContentKind::Mods)
    }

    /// Installed mods, resource packs or shader packs.
    pub fn content(&self, kind: ContentKind) -> Vec<ModFile> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.game_dir().join(kind.dir())) else {
            return out;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let base = name.trim_end_matches(".disabled");
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            // Packs can also be unzipped folders.
            let ok = if is_dir { kind != ContentKind::Mods } else { base.ends_with(kind.ext()) };
            if ok && !name.starts_with('.') {
                let md = e.metadata().ok();
                let size = if is_dir { 0 } else { md.as_ref().map_or(0, |m| m.len()) };
                let modified = md.and_then(|m| m.modified().ok());
                out.push(ModFile { path: e.path(), enabled: !name.ends_with(".disabled"), name, size, modified, is_dir });
            }
        }
        out.sort_by_key(|m| m.display_name().to_lowercase());
        out
    }

    /// Copy files the user dropped into this instance's `kind` folder (reflinked where it
    /// can), returning the names added. Everything is checked before anything is copied:
    /// each must be what the tab lists (`.jar` mods; `.zip` or folder packs), and a name
    /// already there (enabled or not) must hold the same bytes, which is then skipped.
    /// Each call stages its copies in a folder of its own (`.adding-<pid>-<uuid>`), so
    /// drops of the same name, also through a shared folder, never touch each other's
    /// stage, and moves them in without replacing anything, checking both names again.
    pub fn add_content(&self, kind: ContentKind, paths: &[PathBuf]) -> anyhow::Result<Vec<String>> {
        let dir = self.game_dir().join(kind.dir());
        let mut todo = Vec::new();
        for p in paths {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let is_dir = std::fs::metadata(p).with_context(|| format!("can't read {}", p.display()))?.is_dir();
            let fits = if is_dir { kind != ContentKind::Mods } else { name.ends_with(kind.ext()) };
            if !fits || name.starts_with('.') {
                let what = if kind == ContentKind::Mods { ".jar files" } else { ".zip files or folders" };
                anyhow::bail!("{name} isn't a {} ({}s are {what})", kind.noun(), kind.noun());
            }
            match taken(&dir, &name) {
                Some(e) if !is_dir && same_bytes(p, &e) => {}
                Some(_) => anyhow::bail!("a different {name} is already in {}", kind.dir()),
                None if todo.iter().any(|(_, n, _)| *n == name) => anyhow::bail!("two files are named {name}"),
                None => todo.push((p.clone(), name, is_dir)),
            }
        }
        std::fs::create_dir_all(&dir)?;
        clear_stale_stages(&dir);
        let stage = dir.join(format!(".adding-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&stage)?;
        let result = publish_all(kind, &dir, &stage, todo);
        let _ = std::fs::remove_dir_all(&stage);
        result
    }
}

/// The entry already holding `name` in `dir`, enabled or disabled.
fn taken(dir: &Path, name: &str) -> Option<PathBuf> {
    [dir.join(name), dir.join(format!("{name}.disabled"))].into_iter().find(|e| std::fs::symlink_metadata(e).is_ok())
}

/// Copy each of `todo` into `stage` (this call's own) and move it into `dir`. A name
/// another drop took meanwhile is skipped if it holds the same file, else an error.
fn publish_all(kind: ContentKind, dir: &Path, stage: &Path, todo: Vec<(PathBuf, String, bool)>) -> anyhow::Result<Vec<String>> {
    let mut added = Vec::new();
    for (src, name, is_dir) in todo {
        let staged = stage.join(&name);
        let copied = if is_dir { fsutil::copy_tree(&src, &staged, &|_| {}).map(|_| ()) } else { reflink_copy::reflink_or_copy(&src, &staged).map(|_| ()) };
        #[cfg(test)]
        tests::BEFORE_PUBLISH.with(|h| h.borrow().as_ref().map(|h| h()));
        let clash = || anyhow::anyhow!("a different {name} was put in {} meanwhile", kind.dir());
        let moved = copied.map_err(anyhow::Error::from).and_then(|_| match taken(dir, &name) {
            Some(e) if !is_dir && same_bytes(&staged, &e) => Ok(false),
            Some(_) => Err(clash()),
            None => match fsutil::rename_noreplace(&staged, &dir.join(&name)) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && !is_dir && same_bytes(&staged, &dir.join(&name)) => Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(clash()),
                Err(e) => Err(e.into()),
            },
        });
        match moved {
            Ok(true) => added.push(name),
            Ok(false) => {}
            Err(e) => {
                let done = if added.is_empty() { String::new() } else { format!(" (added {} first)", added.join(", ")) };
                return Err(e.context(format!("couldn't add {name}{done}")));
            }
        }
    }
    Ok(added)
}

/// Remove stages left in `dir` by drops whose process is gone (a crash mid-copy). A
/// live process's stages, this one's included, are only ever cleared by their own call.
fn clear_stale_stages(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(".adding-").and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Whether two files hold the same bytes.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else { return false };
    ma.is_file() && mb.is_file() && ma.len() == mb.len() && std::fs::read(a).ok().is_some_and(|x| std::fs::read(b).ok().is_some_and(|y| x == y))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKind {
    Mods,
    ResourcePacks,
    ShaderPacks,
}

impl ContentKind {
    pub fn dir(self) -> &'static str {
        match self {
            ContentKind::Mods => "mods",
            ContentKind::ResourcePacks => "resourcepacks",
            ContentKind::ShaderPacks => "shaderpacks",
        }
    }

    fn ext(self) -> &'static str {
        match self {
            ContentKind::Mods => ".jar",
            _ => ".zip",
        }
    }

    /// Singular noun for messages.
    pub fn noun(self) -> &'static str {
        match self {
            ContentKind::Mods => "mod",
            ContentKind::ResourcePacks => "resource pack",
            ContentKind::ShaderPacks => "shader pack",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModFile {
    pub path: PathBuf,
    pub name: String,
    pub enabled: bool,
    pub size: u64,
    /// Kept across enable/disable (a rename), changed when the file is replaced.
    pub modified: Option<std::time::SystemTime>,
    pub is_dir: bool,
}

impl ModFile {
    pub fn display_name(&self) -> &str {
        self.name.trim_end_matches(".disabled").trim_end_matches(".jar").trim_end_matches(".zip")
    }

    /// Enable or disable by renaming. Refuses if the other name is taken (an
    /// instance can hold both `foo.jar` and `foo.jar.disabled`).
    pub fn toggle(&self) -> anyhow::Result<()> {
        let to = if self.enabled {
            self.path.with_file_name(format!("{}.disabled", self.name))
        } else {
            self.path.with_file_name(self.name.trim_end_matches(".disabled"))
        };
        if std::fs::symlink_metadata(&to).is_ok() {
            anyhow::bail!("{} already exists; remove one of the two first", to.file_name().unwrap_or_default().to_string_lossy());
        }
        std::fs::rename(&self.path, to)?;
        Ok(())
    }
}

fn default_icon() -> String {
    "omacraft".into()
}

/// Game settings files: Minecraft's own and OptiFine's.
pub const SETTINGS_FILES: &[&str] = &["options.txt", "optionsof.txt", "optionsshaders.txt"];

pub fn load_all() -> Vec<Instance> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(paths::instances_dir()) {
        for e in rd.flatten() {
            match Instance::load(&e.path()) {
                Ok(i) => out.push(i),
                Err(err) => eprintln!("skipping {}: {err:#}", e.path().display()),
            }
        }
    }
    sort(&mut out);
    out
}

/// Most recently played first, then by name.
pub fn sort(list: &mut [Instance]) {
    list.sort_by(|a, b| {
        b.last_played
            .cmp(&a.last_played)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    match (h, m) {
        (0, 0) => "never".into(),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Runs in [`add_content`] after each copy, before it's moved in.
        pub static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
    }

    type Job = Box<dyn FnOnce() -> anyhow::Result<Vec<String>> + Send>;

    /// Run two drops on their own threads so that both have copied before either moves
    /// its copy in, then let `a` finish first.
    fn race(a: Job, b: Job) -> (anyhow::Result<Vec<String>>, anyhow::Result<Vec<String>>) {
        let start = |f: Job| {
            let (copied_tx, copied) = std::sync::mpsc::channel();
            let (go, go_rx) = std::sync::mpsc::channel::<()>();
            let t = std::thread::spawn(move || {
                BEFORE_PUBLISH.with(|h| {
                    *h.borrow_mut() = Some(Box::new(move || {
                        copied_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                    }))
                });
                f()
            });
            copied.recv().unwrap();
            (t, go)
        };
        let (ta, go_a) = start(a);
        let (tb, go_b) = start(b);
        go_a.send(()).unwrap();
        let ra = ta.join().unwrap();
        go_b.send(()).unwrap();
        (ra, tb.join().unwrap())
    }

    #[test]
    fn drops_of_the_same_name_never_share_a_stage() {
        let mut inst = Instance::new("Drop Race", "1.21.1", Loader::Fabric("0.16.0".into()));
        inst.assign_id().unwrap();
        let mut other = Instance::new("Drop Race Share", "1.21.1", Loader::Fabric("0.16.0".into()));
        other.assign_id().unwrap();
        let src = paths::test_root().join("drop-race-src");
        let _ = std::fs::remove_dir_all(&src);
        for (d, text) in [("A", "a"), ("B", "b")] {
            std::fs::create_dir_all(src.join(d).join("Pack/assets")).unwrap();
            std::fs::write(src.join(d).join("x.jar"), text).unwrap();
            std::fs::write(src.join(d).join("Pack/assets/f"), text).unwrap();
        }
        let job = |i: &Instance, kind, p: PathBuf| -> Job {
            let i = i.clone();
            Box::new(move || i.add_content(kind, &[p]))
        };

        // Two jars of one name: the first to move in wins with its own bytes.
        let (a, b) = race(job(&inst, ContentKind::Mods, src.join("A/x.jar")), job(&inst, ContentKind::Mods, src.join("B/x.jar")));
        assert_eq!(a.unwrap(), ["x.jar"]);
        assert_eq!(std::fs::read(inst.game_dir().join("mods/x.jar")).unwrap(), b"a");
        assert!(format!("{:#}", b.unwrap_err()).contains("different x.jar"));
        // The same file dropped twice at once is added once, and the other drop skips it.
        std::fs::remove_file(inst.game_dir().join("mods/x.jar")).unwrap();
        let (a, b) = race(job(&inst, ContentKind::Mods, src.join("A/x.jar")), job(&inst, ContentKind::Mods, src.join("A/x.jar")));
        assert_eq!(a.unwrap(), ["x.jar"]);
        assert!(b.unwrap().is_empty());
        // A disabled twin that appears meanwhile is found again when moving in.
        std::fs::remove_file(inst.game_dir().join("mods/x.jar")).unwrap();
        let twin = inst.game_dir().join("mods/x.jar.disabled");
        BEFORE_PUBLISH.with(|h| {
            let (b, twin) = (src.join("B/x.jar"), twin.clone());
            *h.borrow_mut() = Some(Box::new(move || drop(std::fs::copy(&b, &twin))))
        });
        let a = inst.add_content(ContentKind::Mods, &[src.join("A/x.jar")]);
        BEFORE_PUBLISH.with(|h| h.borrow_mut().take());
        assert!(format!("{:#}", a.unwrap_err()).contains("different x.jar"));
        assert!(!inst.game_dir().join("mods/x.jar").exists());
        std::fs::remove_file(&twin).unwrap();

        // Folder packs into one shared folder, through two instances.
        std::fs::create_dir_all(inst.game_dir().join("resourcepacks")).unwrap();
        std::fs::create_dir_all(other.game_dir()).unwrap();
        std::os::unix::fs::symlink(inst.game_dir().join("resourcepacks"), other.game_dir().join("resourcepacks")).unwrap();
        let (a, b) = race(job(&inst, ContentKind::ResourcePacks, src.join("A/Pack")), job(&other, ContentKind::ResourcePacks, src.join("B/Pack")));
        assert_eq!(a.unwrap(), ["Pack"]);
        assert!(b.is_err());
        assert_eq!(std::fs::read(inst.game_dir().join("resourcepacks/Pack/assets/f")).unwrap(), b"a");
        let hidden = |d: &str| std::fs::read_dir(inst.game_dir().join(d)).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with('.'));
        assert!(!hidden("mods") && !hidden("resourcepacks"), "a stage was left behind");
        std::fs::remove_dir_all(inst.dir()).unwrap();
        std::fs::remove_dir_all(other.dir()).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn dropped_content_is_checked_before_anything_is_copied() {
        let mut inst = Instance::new("Drops", "1.21.1", Loader::Fabric("0.16.0".into()));
        inst.assign_id().unwrap();
        let src = paths::test_root().join("drops-src");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join("Pack Folder")).unwrap();
        std::fs::write(src.join("Pack Folder/pack.mcmeta"), "{}").unwrap();
        for (n, b) in [("a.jar", "a"), ("b.jar", "b"), ("notes.txt", "x"), ("p.zip", "p")] {
            std::fs::write(src.join(n), b).unwrap();
        }
        let mods = inst.game_dir().join("mods");
        let has = |n: &str| mods.join(n).exists();

        // One file that doesn't fit stops the whole drop.
        let err = inst.add_content(ContentKind::Mods, &[src.join("a.jar"), src.join("notes.txt")]).unwrap_err();
        assert!(format!("{err:#}").contains("notes.txt"), "{err:#}");
        assert!(!has("a.jar"));
        assert!(inst.add_content(ContentKind::Mods, &[src.join("Pack Folder")]).is_err());

        assert_eq!(inst.add_content(ContentKind::Mods, &[src.join("a.jar"), src.join("b.jar")]).unwrap(), ["a.jar", "b.jar"]);
        assert_eq!(std::fs::read(mods.join("a.jar")).unwrap(), b"a");
        assert!(src.join("a.jar").exists(), "the dropped file stays where it was");

        // The same file again is skipped, also when it's been disabled since.
        std::fs::rename(mods.join("b.jar"), mods.join("b.jar.disabled")).unwrap();
        assert!(inst.add_content(ContentKind::Mods, &[src.join("a.jar"), src.join("b.jar")]).unwrap().is_empty());
        assert!(!has("b.jar"));
        // A different file of the same name is never replaced.
        std::fs::write(src.join("a.jar"), "newer").unwrap();
        assert!(inst.add_content(ContentKind::Mods, &[src.join("a.jar")]).is_err());
        assert_eq!(std::fs::read(mods.join("a.jar")).unwrap(), b"a");

        // Packs take zips and folders.
        assert_eq!(inst.add_content(ContentKind::ResourcePacks, &[src.join("p.zip"), src.join("Pack Folder")]).unwrap(), ["p.zip", "Pack Folder"]);
        assert!(inst.game_dir().join("resourcepacks/Pack Folder/pack.mcmeta").is_file());
        assert!(inst.add_content(ContentKind::ShaderPacks, &[src.join("a.jar")]).is_err());
        // No staged copies are left behind.
        let hidden = |d: &str| std::fs::read_dir(inst.game_dir().join(d)).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with('.'));
        assert!(!hidden("mods") && !hidden("resourcepacks"));
        std::fs::remove_dir_all(inst.dir()).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn assign_id_reserves_distinct_dirs() {
        let mut a = Instance::new("Same Name", "1.21.1", Loader::Vanilla);
        let mut b = a.clone();
        a.assign_id().unwrap();
        b.assign_id().unwrap();
        assert_ne!(a.id, b.id);
        assert!(a.dir().is_dir() && b.dir().is_dir());
        std::fs::remove_dir_all(a.dir()).unwrap();
        std::fs::remove_dir_all(b.dir()).unwrap();
    }

    #[test]
    fn toggle_refuses_to_overwrite() {
        let mut inst = Instance::new("toggle", "1.21.1", Loader::Vanilla);
        inst.assign_id().unwrap();
        let mods = inst.game_dir().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("a.jar"), "new").unwrap();
        std::fs::write(mods.join("a.jar.disabled"), "old").unwrap();
        std::fs::write(mods.join("b.jar"), "b").unwrap();
        let list = inst.mods();
        let a = list.iter().find(|m| m.name == "a.jar").unwrap();
        assert!(a.toggle().is_err());
        assert_eq!(std::fs::read_to_string(mods.join("a.jar.disabled")).unwrap(), "old");
        list.iter().find(|m| m.name == "b.jar").unwrap().toggle().unwrap();
        assert!(mods.join("b.jar.disabled").is_file());
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn copy_settings_takes_only_settings_files() {
        let mut from = Instance::new("settings from", "1.21.1", Loader::Vanilla);
        from.assign_id().unwrap();
        let mut to = Instance::new("settings to", "1.21.1", Loader::Vanilla);
        to.assign_id().unwrap();
        assert!(!from.has_settings());
        std::fs::create_dir_all(from.game_dir().join("config")).unwrap();
        std::fs::write(from.game_dir().join("options.txt"), "fov:0.5\n").unwrap();
        std::fs::write(from.game_dir().join("optionsof.txt"), "ofFogType:3\n").unwrap();
        std::fs::write(from.game_dir().join("servers.dat"), "x").unwrap();
        std::fs::write(from.game_dir().join("config/mod.json"), "{}").unwrap();
        assert!(from.has_settings());
        assert_eq!(to.copy_settings_from(&from).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(to.game_dir().join("options.txt")).unwrap(), "fov:0.5\n");
        assert!(to.game_dir().join("optionsof.txt").is_file());
        assert!(!to.game_dir().join("optionsshaders.txt").exists());
        assert!(!to.game_dir().join("servers.dat").exists() && !to.game_dir().join("config").exists());
        // A copy, not a link: changing one leaves the other alone.
        std::fs::write(to.game_dir().join("options.txt"), "fov:1.0\n").unwrap();
        assert_eq!(std::fs::read_to_string(from.game_dir().join("options.txt")).unwrap(), "fov:0.5\n");
        std::fs::remove_dir_all(from.dir()).unwrap();
        std::fs::remove_dir_all(to.dir()).unwrap();
    }
}
