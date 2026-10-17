//! World sets: instances sharing one `saves` folder (the link itself is shares.rs's).
//!
//! - Every instance in a world set plays the same Minecraft version with the same
//!   loader ([`same_game`]); the UI asks before one changes on its own.
//! - When the game exits, each world it played gets an `omacraft.toml` ([`Record`]):
//!   the instance, Minecraft version, loader and mods (id → version, read from the jars;
//!   Fabric and Quilt worlds don't list their mods anywhere else).
//! - Before launch, every world in the set is checked against the instance's mods
//!   ([`check`]). A world last played with a mod this instance lacks, or a newer
//!   version of one, gets a warning and a snapshot first. Extra mods are fine, and
//!   nothing blocks the launch: content mods can't be told from client-side ones.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backups::{self, Owner};
use crate::content_meta;
use crate::instance::Instance;
use crate::loaders::Loader;
use crate::{fsutil, saves};

/// The file Omacraft keeps in each world folder.
pub const RECORD: &str = "omacraft.toml";

/// What a world was last played with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Instance id and name at the time.
    pub instance: String,
    pub instance_name: String,
    pub mc_version: String,
    pub played: DateTime<Utc>,
    pub loader: Loader,
    /// Mod id → version ("" if the jar doesn't say).
    #[serde(default)]
    pub mods: BTreeMap<String, String>,
}

/// Whether two instances can share worlds: same Minecraft version and loader
/// (the loader's own version may differ).
pub fn same_game(a: &Instance, b: &Instance) -> bool {
    a.mc_version == b.mc_version && a.loader.kind() == b.loader.kind()
}

/// The other instances in `inst`'s world set `set`.
pub fn others<'a>(set: &'a str, id: &'a str, all: &'a [Instance]) -> impl Iterator<Item = &'a Instance> {
    all.iter().filter(move |i| !set.is_empty() && i.world_set == set && i.id != id)
}

/// Why `inst` can't be in world set `set` alongside `all`'s members, if it can't.
pub fn mismatch(inst: &Instance, set: &str, all: &[Instance]) -> Option<String> {
    let other = others(set, &inst.id, all).find(|o| !same_game(inst, o))?;
    Some(format!("“{set}” plays {} ({}); every instance in a world set plays the same version and loader", game_label(other), other.name))
}

/// "1.21.1 with Fabric", or just "1.21.1".
pub fn game_label(i: &Instance) -> String {
    match i.loader {
        Loader::Vanilla => i.mc_version.clone(),
        _ => format!("{} with {}", i.mc_version, i.loader.kind().label()),
    }
}

/// The instance's enabled mods: id → version.
pub fn mods(inst: &Instance) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for m in inst.mods().into_iter().filter(|m| m.enabled) {
        if let Some((id, version)) = content_meta::mod_identity(&m.path) {
            out.entry(id).or_insert(version.unwrap_or_default());
        }
    }
    out
}

pub fn read_record(world: &Path) -> Option<Record> {
    let bytes = fsutil::read_capped(std::fs::File::open(world.join(RECORD)).ok()?).ok()?;
    toml::from_str(&String::from_utf8_lossy(&bytes)).ok()
}

/// Note in each world that `inst` just played it.
pub fn write_records(inst: &Instance, worlds: &[PathBuf]) -> Vec<String> {
    if worlds.is_empty() {
        return Vec::new();
    }
    let rec = Record {
        instance: inst.id.clone(),
        instance_name: inst.name.clone(),
        mc_version: inst.mc_version.clone(),
        played: Utc::now(),
        loader: inst.loader.clone(),
        mods: mods(inst),
    };
    let text = match toml::to_string_pretty(&rec) {
        Ok(t) => t,
        Err(e) => return vec![format!("couldn't record what the worlds were played with: {e}")],
    };
    worlds
        .iter()
        .filter_map(|w| fsutil::write_atomic(&w.join(RECORD), text.as_bytes(), 0o644).err().map(|e| format!("couldn't write {}: {e}", w.join(RECORD).display())))
        .collect()
}

/// Who holds the world's `session.lock` (Minecraft locks it while the world is open):
/// Some(pid) if anyone does, pid -1 if the kernel won't say.
pub fn lock_holder(world: &Path) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(world.join("session.lock")).ok()?;
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as _;
    fl.l_whence = libc::SEEK_SET as _;
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut fl) } != 0 {
        return None;
    }
    (fl.l_type != libc::F_UNLCK as libc::c_short).then_some(fl.l_pid)
}

/// Works out which worlds a game played, for [`write_records`]. Watch it while it
/// runs: a world whose lock this game held is its own. Others in a shared `saves`
/// may be open in another game at the same time, so a world only counts on its
/// `level.dat` changing if no other game was seen holding it.
pub struct PlayWatch {
    saves: PathBuf,
    pid: i32,
    started: SystemTime,
    mine: HashSet<PathBuf>,
    others: HashSet<PathBuf>,
}

impl PlayWatch {
    pub fn new(saves: PathBuf, pid: u32) -> Self {
        Self { saves, pid: pid as i32, started: SystemTime::now(), mine: HashSet::new(), others: HashSet::new() }
    }

    pub fn look(&mut self) {
        for w in world_dirs(&self.saves) {
            match lock_holder(&w) {
                Some(p) if p == self.pid => self.mine.insert(w),
                Some(_) => self.others.insert(w),
                None => false,
            };
        }
    }

    /// The worlds played, once the game has exited.
    pub fn played(self) -> Vec<PathBuf> {
        let changed = |w: &PathBuf| std::fs::metadata(w.join("level.dat")).and_then(|m| m.modified()).is_ok_and(|t| t >= self.started);
        let mut out: Vec<PathBuf> = world_dirs(&self.saves).into_iter().filter(|w| self.mine.contains(w) || (!self.others.contains(w) && changed(w))).collect();
        out.sort();
        out
    }
}

/// World folders (those with a `level.dat`) in `saves`.
fn world_dirs(saves: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(saves).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.join("level.dat").is_file()).collect()
}

/// How a world's record differs from what `inst` would play it with, in ways that can
/// lose things: mods it lacks, mods older than the world has seen, an older Minecraft.
#[derive(Debug, Default, PartialEq)]
pub struct Drift {
    pub missing: Vec<String>,
    /// (mod, world's version, this instance's)
    pub older: Vec<(String, String, String)>,
    /// (world's version, this instance's)
    pub minecraft: Option<(String, String)>,
}

impl Drift {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.older.is_empty() && self.minecraft.is_none()
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some((was, now)) = &self.minecraft {
            parts.push(format!("Minecraft {was} (this instance has {now})"));
        }
        if !self.missing.is_empty() {
            parts.push(format!("mods this instance lacks: {}", self.missing.join(", ")));
        }
        if !self.older.is_empty() {
            let list: Vec<String> = self.older.iter().map(|(m, was, now)| format!("{m} {was} (here {now})")).collect();
            parts.push(format!("newer versions of {}", list.join(", ")));
        }
        parts.join("; ")
    }
}

pub fn drift(rec: &Record, inst_mc: &str, inst_mods: &BTreeMap<String, String>) -> Drift {
    let mut d = Drift::default();
    for (id, was) in &rec.mods {
        match inst_mods.get(id) {
            None => d.missing.push(id.clone()),
            Some(now) if !was.is_empty() && !now.is_empty() && compare_versions(was, now) == Ordering::Greater => d.older.push((id.clone(), was.clone(), now.clone())),
            Some(_) => {}
        }
    }
    if compare_versions(&rec.mc_version, inst_mc) == Ordering::Greater {
        d.minecraft = Some((rec.mc_version.clone(), inst_mc.to_string()));
    }
    d
}

/// Check every world in `inst`'s saves before it launches. Each one last played with
/// something this instance would lose is snapshotted first, unless it's open in
/// another game or a snapshot already covers its last play. Returns warnings for
/// the log. Never fails: the game launches either way.
pub fn check(inst: &Instance) -> Vec<String> {
    let owner = if inst.world_set.is_empty() { Owner::Instance(&inst.id) } else { Owner::Set(&inst.world_set) };
    let here = mods(inst);
    let mut notes = Vec::new();
    for w in saves::list_worlds(&inst.saves_dir()) {
        let Some(rec) = read_record(&w.dir) else { continue };
        let d = drift(&rec, &inst.mc_version, &here);
        if d.is_empty() {
            continue;
        }
        let what = format!("“{}” was last played in {} with {}", w.name, rec.instance_name, d.describe());
        let modified = std::fs::metadata(w.dir.join("level.dat")).and_then(|m| m.modified()).ok().map(DateTime::<Utc>::from);
        let covered = backups::snapshots(owner, &w.folder).into_iter().find(|s| modified.is_some_and(|m| s.info.taken >= m));
        if let Some(s) = covered {
            notes.push(format!("{what}. The snapshot from {} still covers it: {}", s.info.taken.format("%Y-%m-%d %H:%M"), s.dir.display()));
        } else if lock_holder(&w.dir).is_some() {
            notes.push(format!("{what}. It's open in another game, so no snapshot was taken"));
        } else {
            match backups::snapshot_world(&w.dir, owner, &format!("before {} played it: {}", inst.name, d.describe())) {
                Ok(dir) => notes.push(format!("{what}. Snapshot taken first: {}", dir.display())),
                Err(e) => notes.push(format!("{what}. The snapshot failed: {e:#}")),
            }
        }
    }
    notes
}

/// Compare two version strings leniently: digit runs as numbers, other runs as text,
/// build metadata (`+…`) ignored, and a pre-release (`-beta.2`, `-rc1`) before its
/// release. Good enough for mod versions, which follow no one scheme.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Part {
        Text(String),
        Num(u64),
    }
    let parts = |s: &str| -> Vec<Part> {
        let s = s.split('+').next().unwrap_or(s);
        let mut out = Vec::new();
        let mut cur = String::new();
        let flush = |cur: &mut String, out: &mut Vec<Part>| {
            if !cur.is_empty() {
                let p = if cur.chars().all(|c| c.is_ascii_digit()) { Part::Num(cur.parse().unwrap_or(u64::MAX)) } else { Part::Text(cur.to_lowercase()) };
                out.push(p);
                cur.clear();
            }
        };
        for c in s.chars() {
            if c.is_alphanumeric() {
                if cur.chars().last().is_some_and(|l| l.is_ascii_digit() != c.is_ascii_digit()) {
                    flush(&mut cur, &mut out);
                }
                cur.push(c);
            } else {
                flush(&mut cur, &mut out);
            }
        }
        flush(&mut cur, &mut out);
        out
    };
    let pre = |p: &Part| matches!(p, Part::Text(t) if ["alpha", "beta", "pre", "rc", "snapshot", "a", "b"].contains(&t.as_str()));
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        match (pa.get(i), pb.get(i)) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return x.cmp(y),
            // 1.0-beta < 1.0 < 1.0.1
            (Some(x), None) => return if pre(x) { Ordering::Less } else { Ordering::Greater },
            (None, Some(y)) => return if pre(y) { Ordering::Greater } else { Ordering::Less },
            (None, None) => break,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn versions_compare_like_people_read_them() {
        use Ordering::*;
        let cases = [
            ("1.2.10", "1.2.9", Greater),
            ("0.6.0", "0.6.0-beta.2", Greater),
            ("0.6.0-beta.2", "0.6.0-beta.10", Less),
            ("mc1.21.1-0.6.1-fabric", "mc1.21.1-0.6.0-fabric", Greater),
            ("0.16.5+build.1", "0.16.5+build.7", Equal),
            ("1.21.1", "1.21", Greater),
            ("1.21-pre1", "1.21", Less),
            ("2.0.0", "2.0.0", Equal),
        ];
        for (a, b, want) in cases {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn a_world_set_plays_one_version_and_loader() {
        let inst = |id: &str, mc: &str, loader: Loader| {
            let mut i = Instance::new(id, mc, loader);
            i.id = id.into();
            i.world_set = "s".into();
            i
        };
        let all = [inst("a", "1.21.1", Loader::Fabric("0.16.5".into())), inst("b", "1.21.1", Loader::Fabric("0.16.9".into()))];
        // The loader's own version may differ.
        assert!(mismatch(&all[1], "s", &all).is_none());
        assert!(mismatch(&inst("c", "1.21.1", Loader::Quilt("0.27".into())), "s", &all).is_some());
        assert!(mismatch(&inst("d", "1.21.4", Loader::Fabric("0.16.5".into())), "s", &all).is_some());
        // A lone member, or no set, never clashes.
        assert!(mismatch(&inst("a", "1.20.1", Loader::Vanilla), "s", &all[..1]).is_none());
        assert!(mismatch(&inst("e", "1.20.1", Loader::Vanilla), "", &all).is_none());
    }

    fn rec(mc: &str, mods: &[(&str, &str)]) -> Record {
        Record {
            instance: "a".into(),
            instance_name: "A".into(),
            mc_version: mc.into(),
            played: Utc::now(),
            loader: Loader::Fabric("0.16.5".into()),
            mods: mods.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn only_lost_or_older_mods_count() {
        let here: BTreeMap<String, String> = [("sodium", "0.6.0"), ("create", "6.0.0"), ("extra", "1"), ("odd", "")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        // Extra mods here, the same or newer versions: nothing to lose.
        assert!(drift(&rec("1.21.1", &[("sodium", "0.5.0"), ("create", "6.0.0")]), "1.21.1", &here).is_empty());
        let d = drift(&rec("1.21.4", &[("create", "6.1.0"), ("botania", "1"), ("odd", "3")]), "1.21.1", &here);
        assert_eq!(d.missing, ["botania"]);
        assert_eq!(d.older, [("create".to_string(), "6.1.0".to_string(), "6.0.0".to_string())]);
        assert_eq!(d.minecraft, Some(("1.21.4".into(), "1.21.1".into())));
    }

    /// A jar with a fabric.mod.json, as `mods/<file>`.
    fn fabric_jar(inst: &Instance, file: &str, id: &str, version: &str) {
        let dir = inst.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(dir.join(file)).unwrap());
        z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap();
        write!(z, r#"{{"schemaVersion": 1, "id": "{id}", "version": "{version}", "name": "{id}"}}"#).unwrap();
        z.finish().unwrap();
    }

    fn world(saves: &Path, folder: &str) -> PathBuf {
        let w = saves.join(folder);
        std::fs::create_dir_all(&w).unwrap();
        // list_worlds wants a level.dat; an unreadable one still lists by folder name.
        std::fs::write(w.join("level.dat"), "").unwrap();
        w
    }

    #[test]
    fn records_and_checks_round_trip_with_one_snapshot_per_play() {
        let mut a = Instance::new("rec a", "1.21.1", Loader::Fabric("0.16.5".into()));
        a.assign_id().unwrap();
        a.world_set = format!("ws {}", uuid::Uuid::new_v4().simple());
        fabric_jar(&a, "create.jar", "create", "6.1.0");
        fabric_jar(&a, "sodium.jar", "sodium", "0.6.0");
        fabric_jar(&a, "off.jar.disabled", "off", "1");
        let w = world(&a.saves_dir(), "Base");
        let calm = world(&a.saves_dir(), "Calm");
        assert!(write_records(&a, &[w.clone()]).is_empty());
        let r = read_record(&w).unwrap();
        assert_eq!(r.mods.keys().collect::<Vec<_>>(), ["create", "sodium"], "disabled mods don't count");
        assert_eq!(r.instance, a.id);

        // The same instance finds nothing to warn about.
        assert!(check(&a).is_empty());

        // An instance with an older create and no sodium: warned, snapshotted once.
        let mut b = a.clone();
        b.name = "rec b".into();
        std::fs::remove_file(b.game_dir().join("mods/sodium.jar")).unwrap();
        fabric_jar(&b, "create.jar", "create", "6.0.0");
        let notes = check(&b);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("sodium") && notes[0].contains("create 6.1.0") && notes[0].contains("Snapshot taken"), "{}", notes[0]);
        let owner = Owner::Set(&b.world_set);
        assert_eq!(backups::snapshots(owner, "Base").len(), 1);
        assert!(backups::snapshots(owner, "Calm").is_empty(), "worlds without a record are left alone");
        // Launching again without playing it takes no second snapshot.
        assert!(check(&b)[0].contains("still covers it"));
        assert_eq!(backups::snapshots(owner, "Base").len(), 1);
        let _ = calm;

        std::fs::remove_dir_all(a.dir()).unwrap();
        std::fs::remove_dir_all(backups::dir().join("sets").join(&a.world_set)).unwrap();
    }

    #[test]
    fn a_game_counts_the_worlds_it_held_and_not_another_games() {
        let root = crate::paths::test_root().join(format!("watch-{}", uuid::Uuid::new_v4().simple()));
        let saves = root.join("saves");
        let mine = world(&saves, "Mine");
        let theirs = world(&saves, "Theirs");
        let old = world(&saves, "Old");
        let past = std::fs::File::options().write(true).open(old.join("level.dat")).unwrap();
        past.set_modified(SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();

        // Two "games", each holding one world's lock the way Java's FileChannel.tryLock does.
        let game = Holder::lock(&mine);
        let other = Holder::lock(&theirs);
        assert_eq!(lock_holder(&mine), Some(game.pid()));
        assert_eq!(lock_holder(&theirs), Some(other.pid()));
        let mut watch = PlayWatch::new(saves.clone(), game.pid() as u32);
        // File times come from the kernel's coarse clock, a tick behind `started`; no game
        // writes a world within milliseconds of launch.
        std::thread::sleep(std::time::Duration::from_millis(50));
        watch.look();
        drop((game, other));
        assert_eq!(lock_holder(&mine), None);
        // Both level.dats change during the run; only "Mine" was this game's. A world
        // nobody was seen holding counts by its level.dat (older versions don't lock).
        std::fs::write(mine.join("level.dat"), "x").unwrap();
        std::fs::write(theirs.join("level.dat"), "x").unwrap();
        let fresh = world(&saves, "Fresh");
        assert_eq!(watch.played(), [fresh, mine]);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A child process holding a POSIX write lock on a world's session.lock until dropped:
    /// this test binary again, running [`hold_lock_helper`].
    struct Holder(std::process::Child);

    impl Holder {
        fn lock(world: &Path) -> Self {
            use std::io::BufRead;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "world_sets::tests::hold_lock_helper", "--ignored", "--nocapture", "--test-threads=1"])
                .env("OMACRAFT_TEST_LOCK", world.join("session.lock"))
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let out = std::io::BufReader::new(child.stdout.take().unwrap());
            // libtest prints the test's name first, on the same line.
            assert!(out.lines().map_while(Result::ok).any(|l| l.ends_with("omacraft-test-locked")), "helper couldn't lock");
            Holder(child)
        }

        fn pid(&self) -> i32 {
            self.0.id() as i32
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Run by [`Holder`] in a child process: lock the file, as Java's FileChannel.tryLock
    /// does, and wait to be killed.
    #[test]
    #[ignore = "helper, run by Holder in a child process"]
    fn hold_lock_helper() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let Some(path) = std::env::var_os("OMACRAFT_TEST_LOCK") else { return };
        let f = std::fs::File::options().create(true).truncate(false).read(true).write(true).open(path).unwrap();
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        assert_eq!(unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &fl) }, 0);
        println!("omacraft-test-locked");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
