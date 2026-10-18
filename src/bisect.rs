//! Finding the breaking mod: a guided bisect over an instance's enabled mods.
//!
//! - Each step turns about half the suspects on, plus everything they and the kept
//!   mods require ([`Bisect::closure`], from the jars' own dependency lists), so no mod
//!   is ever on without what it needs. The player plays and says whether the problem
//!   happened: if it did, the culprit is among the suspects that were on; if not, among
//!   those that were off. "Can't tell" tries a different split.
//! - With one suspect left, a last step turns on everything but it (and what needs it)
//!   to confirm the problem goes away. If it doesn't, it wasn't one mod alone.
//! - The state lives in `bisect.toml` in the instance folder, so it survives a restart,
//!   and the mods are put back exactly as they were when it started ([`restore`]).
//!
//! Mods are named by file name as if enabled (`sodium-0.6.0.jar`), since nothing
//! else changes the mods folder while a bisect runs (the mods tab is locked).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instance::Instance;
use crate::{content_meta, fsutil, mod_sets, world_sets};

pub const FILE: &str = "bisect.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    /// The problem happened.
    Broken,
    /// It didn't.
    Works,
    /// Can't tell (the game didn't get far enough, say).
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    /// Some of the suspects on, to halve them.
    Narrow,
    /// Everything but the last suspects on, to check the problem goes away.
    Confirm,
}

/// The mods on for the step being played.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub kind: StepKind,
    pub on: BTreeSet<String>,
    pub began: DateTime<Utc>,
}

/// A step answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tried {
    pub kind: StepKind,
    /// The suspects that were on.
    pub tested: BTreeSet<String>,
    pub answer: Answer,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Culprit {
    pub file: String,
    /// From the jar, or Modrinth.
    pub name: Option<String>,
    pub version: Option<String>,
    /// Modrinth project id, for its page.
    pub project: Option<String>,
}

impl Culprit {
    pub fn label(&self) -> String {
        match (&self.name, &self.version) {
            (Some(n), Some(v)) => format!("{n} {v}"),
            (Some(n), None) => n.clone(),
            _ => self.file.trim_end_matches(".jar").to_string(),
        }
    }
}

/// How it ended.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Done {
    /// Empty if it couldn't pin the problem on a mod.
    pub culprits: Vec<Culprit>,
    /// The last check (everything but the culprits on) passed.
    pub confirmed: bool,
    /// Why it ended this way, for the player.
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bisect {
    pub started: DateTime<Utc>,
    /// Every mod file when it started → whether it was on: put back exactly at the end.
    pub original: BTreeMap<String, bool>,
    /// On in every step and never suspected.
    #[serde(default)]
    pub keep: BTreeSet<String>,
    /// Still possibly to blame, the ones to try first first.
    pub suspects: Vec<String>,
    /// How many suspects it started with.
    pub total: usize,
    /// Each enabled file → the enabled files it requires.
    #[serde(default)]
    pub needs: BTreeMap<String, BTreeSet<String>>,
    /// Splits skipped in a row, so the next one differs.
    #[serde(default)]
    pub skips: usize,
    #[serde(default)]
    pub history: Vec<Tried>,
    pub step: Option<Step>,
    pub done: Option<Done>,
}

/// What to start from: by default every enabled mod is a suspect.
#[derive(Debug, Clone, Default)]
pub struct Start {
    /// Mods on in every step and never suspected (libraries, the mod you're testing for).
    pub keep: Vec<String>,
    /// Mods known not to be the problem: off unless something needs them.
    pub good: Vec<String>,
    /// Suspects to try first, e.g. the mods a crash report names.
    pub first: Vec<String>,
}

/// A file's name as if enabled.
pub fn base(name: &str) -> String {
    name.trim_end_matches(".disabled").to_string()
}

pub fn path(inst: &Instance) -> PathBuf {
    inst.dir().join(FILE)
}

/// The bisect running in `inst`, if one is.
pub fn load(inst: &Instance) -> anyhow::Result<Option<Bisect>> {
    let p = path(inst);
    match std::fs::read(&p) {
        Ok(bytes) => Ok(Some(toml::from_str(&String::from_utf8_lossy(&bytes)).with_context(|| format!("reading {}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save(inst: &Instance, b: &Bisect) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(b)?;
    fsutil::write_atomic(&path(inst), text.as_bytes(), 0o644)?;
    Ok(())
}

/// The mod ids a jar answers to, and the ones it requires. Platform ids (minecraft,
/// the loader, java) come back too; nothing provides them, so they lead nowhere.
pub fn jar_links(jar: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    use serde_json::Value;
    let mut provides: BTreeSet<String> = world_sets::mod_inventory(jar).into_keys().collect();
    let mut requires = BTreeSet::new();
    let Ok(mut zip) = std::fs::File::open(jar).map_err(anyhow::Error::from).and_then(|f| Ok(zip::ZipArchive::new(f)?)) else {
        return (provides, requires);
    };
    let json = |zip: &mut zip::ZipArchive<std::fs::File>, name| content_meta::zip_bytes(zip, name).and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let strings = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|x| x.as_str().or(x["id"].as_str())).map(String::from).collect() };
    if let Some(f) = json(&mut zip, "fabric.mod.json") {
        provides.extend(strings(&f["provides"]));
        requires.extend(f["depends"].as_object().into_iter().flatten().map(|(k, _)| k.clone()));
    } else if let Some(q) = json(&mut zip, "quilt.mod.json") {
        let l = &q["quilt_loader"];
        provides.extend(strings(&l["provides"]));
        for d in l["depends"].as_array().into_iter().flatten() {
            match d {
                Value::String(id) => {
                    requires.insert(id.clone());
                }
                Value::Object(o) if !o.get("optional").and_then(Value::as_bool).unwrap_or(false) => {
                    requires.extend(o.get("id").and_then(Value::as_str).map(String::from));
                }
                _ => {}
            }
        }
    } else if let Some(t) = ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"]
        .iter()
        .find_map(|n| content_meta::zip_bytes(&mut zip, n).and_then(|b| toml::from_str::<toml::Value>(&String::from_utf8_lossy(&b)).ok()))
    {
        // [[dependencies.<modid>]]: required if `type = "required"` (NeoForge) or `mandatory = true` (Forge).
        let lists: Vec<&toml::Value> = match t.get("dependencies") {
            Some(toml::Value::Table(by_mod)) => by_mod.values().collect(),
            Some(v @ toml::Value::Array(_)) => vec![v],
            _ => Vec::new(),
        };
        for d in lists.into_iter().filter_map(|l| l.as_array()).flatten() {
            let required = match d.get("type").and_then(|v| v.as_str()) {
                Some(kind) => kind.eq_ignore_ascii_case("required"),
                None => d.get("mandatory").and_then(|v| v.as_bool()).unwrap_or(false),
            };
            if required && let Some(id) = d.get("modId").and_then(|v| v.as_str()) {
                requires.insert(id.to_string());
            }
        }
    } else if let Some(info) = json(&mut zip, "mcmod.info") {
        let list = if info.is_array() { &info } else { &info["modList"] };
        for m in list.as_array().into_iter().flatten() {
            requires.extend(strings(&m["requiredMods"]).into_iter().map(|r| r.split('@').next().unwrap_or_default().to_string()));
        }
    }
    for id in &provides {
        requires.remove(id);
    }
    (provides, requires)
}

/// Start finding the breaking mod in `inst`: every enabled mod not kept or known good
/// is a suspect. Turns the first step's mods on and the rest off, and saves the state.
/// Reads every enabled jar: run it off the UI thread.
pub fn start(inst: &Instance, opts: &Start) -> anyhow::Result<Bisect> {
    if load(inst)?.is_some() {
        bail!("it's already finding a breaking mod; stop that one first");
    }
    let files: Vec<_> = inst.mods().into_iter().filter(|f| !f.is_dir).collect();
    let original: BTreeMap<String, bool> = files.iter().map(|f| (base(&f.name), f.enabled)).collect();
    let enabled: Vec<&crate::instance::ModFile> = files.iter().filter(|f| f.enabled).collect();
    let links: Vec<(String, BTreeSet<String>, BTreeSet<String>)> = enabled
        .iter()
        .map(|f| {
            let (p, r) = jar_links(&f.path);
            (base(&f.name), p, r)
        })
        .collect();
    let mut providers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (name, provides, _) in &links {
        for id in provides {
            providers.entry(id).or_default().insert(name);
        }
    }
    let needs: BTreeMap<String, BTreeSet<String>> = links
        .iter()
        .map(|(name, _, requires)| {
            let by: BTreeSet<String> = requires.iter().filter_map(|id| providers.get(id.as_str())).flatten().filter(|p| **p != name.as_str()).map(|p| p.to_string()).collect();
            (name.clone(), by)
        })
        .collect();

    let on: BTreeSet<String> = enabled.iter().map(|f| base(&f.name)).collect();
    let keep: BTreeSet<String> = opts.keep.iter().map(|k| base(k)).filter(|k| on.contains(k)).collect();
    let good: BTreeSet<String> = opts.good.iter().map(|k| base(k)).collect();
    let mut suspects: Vec<String> = Vec::new();
    for f in opts.first.iter().map(|f| base(f)).chain(on.iter().cloned()) {
        if on.contains(&f) && !keep.contains(&f) && !good.contains(&f) && !suspects.contains(&f) {
            suspects.push(f);
        }
    }
    if suspects.is_empty() {
        bail!("there's nothing to test: every enabled mod is kept on or marked fine");
    }
    let mut b = Bisect {
        started: Utc::now(),
        original,
        keep,
        total: suspects.len(),
        suspects,
        needs,
        skips: 0,
        history: Vec::new(),
        step: None,
        done: None,
    };
    b.step = Some(b.plan());
    apply_step(inst, &b)?;
    save(inst, &b)?;
    Ok(b)
}

impl Bisect {
    /// `seed` and everything it requires, among the mods on at the start.
    pub fn closure<'a>(&self, seed: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = seed.into_iter().cloned().collect();
        while let Some(f) = queue.pop_front() {
            if out.insert(f.clone()) {
                queue.extend(self.needs.get(&f).into_iter().flatten().filter(|d| !out.contains(*d)).cloned());
            }
        }
        out
    }

    /// The mods on at the start that need any of `these`, directly or not.
    pub fn dependents(&self, these: &BTreeSet<String>) -> BTreeSet<String> {
        let mut out = these.clone();
        loop {
            let more: Vec<String> = self.needs.iter().filter(|(f, deps)| !out.contains(*f) && deps.iter().any(|d| out.contains(d))).map(|(f, _)| f.clone()).collect();
            if more.is_empty() {
                break;
            }
            out.extend(more);
        }
        out.retain(|f| !these.contains(f));
        out
    }

    fn originally_on(&self) -> BTreeSet<String> {
        self.original.iter().filter(|(_, on)| **on).map(|(f, _)| f.clone()).collect()
    }

    /// The next step: the split of the suspects closest to half (counting what each
    /// side drags in), different after a skip; or the confirming step once one is left
    /// or the rest can't be told apart.
    pub fn plan(&self) -> Step {
        let n = self.suspects.len();
        let suspects: BTreeSet<&String> = self.suspects.iter().collect();
        let hits = |on: &BTreeSet<String>| on.iter().filter(|f| suspects.contains(f)).count();
        // Closest to half wins; a split that's all or nothing tells nothing.
        let consider = |best: &mut Option<(BTreeSet<String>, usize)>, on: BTreeSet<String>| {
            let h = hits(&on);
            let score = |h: usize| (2 * h).abs_diff(n);
            if h > 0 && h < n && best.as_ref().is_none_or(|(_, b)| score(h) < score(*b)) {
                *best = Some((on, h));
            }
            h
        };
        let mut best = None;
        if n > 1 {
            let order: Vec<&String> = self.suspects.iter().cycle().skip(self.skips % n).take(n).collect();
            for k in 1..=n {
                let on = self.closure(order[..k].iter().copied().chain(&self.keep));
                if 2 * consider(&mut best, on) >= n && best.is_some() {
                    break;
                }
            }
            if best.is_none() {
                for s in &order {
                    consider(&mut best, self.closure(std::iter::once(*s).chain(&self.keep)));
                }
            }
        }
        match best {
            Some((on, _)) => Step { kind: StepKind::Narrow, on, began: Utc::now() },
            None => {
                // Everything but the suspects (and what needs them).
                let out: BTreeSet<String> = self.suspects.iter().cloned().collect();
                let gone = self.dependents(&out);
                let on = self.originally_on().into_iter().filter(|f| !out.contains(f) && !gone.contains(f)).collect();
                Step { kind: StepKind::Confirm, on, began: Utc::now() }
            }
        }
    }

    /// Take the answer for the current step and plan the next, or finish.
    pub fn answer(&mut self, a: Answer) -> anyhow::Result<()> {
        let step = self.step.take().context("there's no step waiting for an answer")?;
        let tested: BTreeSet<String> = self.suspects.iter().filter(|s| step.on.contains(*s)).cloned().collect();
        self.history.push(Tried { kind: step.kind, tested, answer: a });
        let culprits = |b: &Bisect| b.suspects.iter().map(|f| Culprit { file: f.clone(), ..Default::default() }).collect();
        match (step.kind, a) {
            (StepKind::Narrow, Answer::Broken) => {
                self.suspects.retain(|s| step.on.contains(s));
                self.skips = 0;
            }
            (StepKind::Narrow, Answer::Works) => {
                self.suspects.retain(|s| !step.on.contains(s));
                self.skips = 0;
            }
            (StepKind::Narrow, Answer::Skip) => {
                self.skips += 1;
                if self.skips > self.suspects.len() {
                    self.done = Some(Done { culprits: Vec::new(), confirmed: false, note: "Too many steps couldn't be told apart to narrow the problem down.".into() });
                }
            }
            (StepKind::Confirm, Answer::Works) => {
                self.done = Some(Done { culprits: culprits(self), confirmed: true, note: "With this mod off, the problem went away.".into() });
            }
            (StepKind::Confirm, Answer::Skip) => {
                self.done = Some(Done { culprits: culprits(self), confirmed: false, note: "This is the likely mod, though the last check couldn't confirm it.".into() });
            }
            (StepKind::Confirm, Answer::Broken) => {
                self.done = Some(Done {
                    culprits: Vec::new(),
                    confirmed: false,
                    note: "The problem occurred even with the last suspect off, so no single mod causes it. The problem may need two mods together, come from a mod you kept on, or not occur every time.".into(),
                });
            }
        }
        if self.done.is_none() {
            if self.suspects.is_empty() {
                self.done = Some(Done {
                    culprits: Vec::new(),
                    confirmed: false,
                    note: "Every half worked on its own, so no single mod causes the problem. The problem may need two mods together, or not occur every time.".into(),
                });
            } else {
                self.step = Some(self.plan());
            }
        }
        Ok(())
    }

    /// (this step's number, about how many there'll be in all).
    pub fn progress(&self) -> (usize, usize) {
        let done = self.history.len();
        let left = match &self.step {
            None => 0,
            Some(s) if s.kind == StepKind::Confirm => 1,
            Some(_) => (usize::BITS - (self.suspects.len() - 1).leading_zeros()) as usize + 1,
        };
        (done + 1, done + left)
    }
}

/// Turn `inst`'s mods on and off for the current step (or as they were, if none).
pub fn apply_step(inst: &Instance, b: &Bisect) -> anyhow::Result<()> {
    let on = match &b.step {
        Some(s) => s.on.clone(),
        None => b.originally_on(),
    };
    set_mods(inst, b, &on)
}

/// Every file that was there at the start on if in `on`, else off. Files added since stay as they are.
fn set_mods(inst: &Instance, b: &Bisect, on: &BTreeSet<String>) -> anyhow::Result<()> {
    let mut failed = Vec::new();
    for f in inst.mods().into_iter().filter(|f| !f.is_dir) {
        let name = base(&f.name);
        if b.original.contains_key(&name)
            && let Err(e) = mod_sets::rename(&f.path, on.contains(&name))
        {
            failed.push(format!("{e:#}"));
        }
    }
    if !failed.is_empty() {
        bail!("couldn't switch every mod: {}", failed.join("; "));
    }
    Ok(())
}

/// Answer the current step, then switch to the next one. Saves the state.
pub fn answer(inst: &Instance, a: Answer) -> anyhow::Result<Bisect> {
    let mut b = load(inst)?.context("there's no search for a breaking mod running")?;
    b.answer(a)?;
    if b.done.is_some() {
        // Back to how it was while the result is read; the player picks what to keep.
        set_mods(inst, &b, &b.originally_on())?;
        describe(inst, &mut b);
    } else {
        apply_step(inst, &b)?;
    }
    save(inst, &b)?;
    Ok(b)
}

/// Fill in the culprits' names and versions from their jars.
fn describe(inst: &Instance, b: &mut Bisect) {
    let dir = inst.game_dir().join("mods");
    for c in b.done.iter_mut().flat_map(|d| d.culprits.iter_mut()) {
        let p = [dir.join(&c.file), dir.join(format!("{}.disabled", c.file))].into_iter().find(|p| p.is_file());
        if let Some(p) = p {
            let m = content_meta::read_local(&p, false, crate::instance::ContentKind::Mods);
            c.name = m.name;
            c.version = m.version;
        }
    }
}

/// Put the mods back exactly as they were when it started and forget the bisect.
pub fn restore(inst: &Instance) -> anyhow::Result<()> {
    let Some(b) = load(inst)? else { return Ok(()) };
    set_mods(inst, &b, &b.originally_on())?;
    std::fs::remove_file(path(inst)).context("forgetting the search")?;
    Ok(())
}

/// Restore, then turn the culprits off, with whatever needs them. Returns the mods
/// turned off besides the culprits.
pub fn disable_culprits(inst: &Instance) -> anyhow::Result<Vec<String>> {
    let b = load(inst)?.context("there's no search for a breaking mod running")?;
    let culprits: BTreeSet<String> = b.done.iter().flat_map(|d| &d.culprits).map(|c| c.file.clone()).collect();
    let also = b.dependents(&culprits);
    let on: BTreeSet<String> = b.originally_on().into_iter().filter(|f| !culprits.contains(f) && !also.contains(f)).collect();
    set_mods(inst, &b, &on)?;
    std::fs::remove_file(path(inst)).context("forgetting the search")?;
    Ok(also.into_iter().collect())
}

/// Whether a crash report appeared in `inst` since `since`.
pub fn crashed_since(inst: &Instance, since: DateTime<Utc>) -> bool {
    std::fs::read_dir(inst.crash_reports_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .any(|t| DateTime::<Utc>::from(t) > since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;
    use crate::mod_sets::tests::jar;

    fn inst(name: &str) -> Instance {
        let mut i = Instance::new(name, "1.21.1", Loader::Fabric("0.16.5".into()));
        i.assign_id().unwrap();
        i
    }

    fn on_now(inst: &Instance) -> BTreeSet<String> {
        inst.mods().into_iter().filter(|m| m.enabled).map(|m| m.name).collect()
    }

    /// A Forge jar declaring `id` and its dependencies (`(id, required)`).
    fn forge_jar(inst: &Instance, name: &str, id: &str, deps: &[(&str, bool)]) {
        use std::io::Write;
        let dir = inst.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(dir.join(name)).unwrap());
        z.start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default()).unwrap();
        writeln!(z, "modLoader=\"javafml\"\n[[mods]]\nmodId=\"{id}\"\nversion=\"1.0\"").unwrap();
        writeln!(z, "[[dependencies.{id}]]\nmodId=\"forge\"\nmandatory=true").unwrap();
        for (d, req) in deps {
            // Old Forge says `mandatory`; NeoForge says `type`.
            if *req {
                writeln!(z, "[[dependencies.{id}]]\nmodId=\"{d}\"\ntype=\"required\"").unwrap();
            } else {
                writeln!(z, "[[dependencies.{id}]]\nmodId=\"{d}\"\nmandatory=false").unwrap();
            }
        }
        z.finish().unwrap();
    }

    #[test]
    fn dependencies_come_from_fabric_and_forge_jars() {
        let i = inst("bisect deps");
        let lib = jar(&i, "lib-1.0.jar", Some("lib"), &[]);
        let app = jar(&i, "app-1.0.jar", Some("app"), &["lib", "fabricloader"]);
        assert_eq!(jar_links(&app).1, BTreeSet::from(["fabricloader".into(), "lib".into(), "minecraft".into()]));
        assert!(jar_links(&lib).0.contains("lib"));
        forge_jar(&i, "core-1.jar", "core", &[]);
        forge_jar(&i, "addon-1.jar", "addon", &[("core", true), ("jei", false)]);
        let (provides, requires) = jar_links(&i.game_dir().join("mods/addon-1.jar"));
        assert_eq!(provides, BTreeSet::from(["addon".into()]));
        assert_eq!(requires, BTreeSet::from(["core".into(), "forge".into()]), "optional ones don't count");

        let b = start(&i, &Start::default()).unwrap();
        assert_eq!(b.needs["app-1.0.jar"], BTreeSet::from(["lib-1.0.jar".into()]));
        assert_eq!(b.needs["addon-1.jar"], BTreeSet::from(["core-1.jar".into()]));
        assert!(b.needs["lib-1.0.jar"].is_empty());
        // Whatever a step turns on, it turns on what that needs.
        for f in &b.step.as_ref().unwrap().on {
            for d in &b.needs[f] {
                assert!(b.step.as_ref().unwrap().on.contains(d), "{f} without {d}");
            }
        }
        assert_eq!(b.closure([&"addon-1.jar".to_string()]), BTreeSet::from(["addon-1.jar".into(), "core-1.jar".into()]));
        restore(&i).unwrap();
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    /// Plays every step against `bad` (a set of files that breaks the game if all on;
    /// `None` = never breaks), answering honestly. Returns how it ended.
    fn run(i: &Instance, opts: &Start, broken: impl Fn(&BTreeSet<String>) -> bool) -> Bisect {
        let mut b = start(i, opts).unwrap();
        let mut steps = 0;
        while b.done.is_none() {
            steps += 1;
            assert!(steps < 40, "never finished");
            let on: BTreeSet<String> = on_now(i).into_iter().map(|n| base(&n)).collect();
            assert_eq!(on, b.step.as_ref().unwrap().on, "the folder matches the step");
            b = answer(i, if broken(&on) { Answer::Broken } else { Answer::Works }).unwrap();
        }
        b
    }

    #[test]
    fn narrows_down_to_one_mod_and_restores_exactly() {
        let i = inst("bisect narrow");
        jar(&i, "lib-1.jar", Some("lib"), &[]);
        for n in 0..13 {
            let needs: &[&str] = if n % 3 == 0 { &["lib"] } else { &[] };
            jar(&i, &format!("m{n:02}-1.jar"), Some(&format!("m{n:02}")), needs);
        }
        jar(&i, "off-1.jar.disabled", Some("off"), &[]);
        let before = on_now(&i);

        let b = run(&i, &Start::default(), |on| on.contains("m09-1.jar"));
        let done = b.done.unwrap();
        assert_eq!(done.culprits.iter().map(|c| c.file.as_str()).collect::<Vec<_>>(), ["m09-1.jar"]);
        assert!(done.confirmed);
        assert!(b.history.len() <= 6, "{} steps for 14 mods", b.history.len());
        assert_eq!(on_now(&i), before, "back as it was while the result shows");

        // A culprit that's a library others need: found too, and turning it off takes them with it.
        restore(&i).unwrap();
        let b = run(&i, &Start::default(), |on| on.contains("lib-1.jar"));
        assert_eq!(b.done.as_ref().unwrap().culprits[0].file, "lib-1.jar");
        let also = disable_culprits(&i).unwrap();
        assert_eq!(also, ["m00-1.jar", "m03-1.jar", "m06-1.jar", "m09-1.jar", "m12-1.jar"]);
        let now = on_now(&i);
        assert!(!now.contains("lib-1.jar") && !now.contains("m03-1.jar") && now.contains("m01-1.jar"));
        assert!(load(&i).unwrap().is_none());
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn kept_and_good_mods_and_the_ones_to_try_first() {
        let i = inst("bisect marks");
        for n in 0..6 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let opts = Start { keep: vec!["m0-1.jar".into()], good: vec!["m1-1.jar".into()], first: vec!["m5-1.jar".into()] };
        let b = start(&i, &opts).unwrap();
        assert_eq!(b.suspects, ["m5-1.jar", "m2-1.jar", "m3-1.jar", "m4-1.jar"]);
        let step = b.step.clone().unwrap();
        assert!(step.on.contains("m0-1.jar") && !step.on.contains("m1-1.jar"), "kept on, good off");
        assert!(step.on.contains("m5-1.jar"), "suspects first are tried first");
        restore(&i).unwrap();
        // Nothing to suspect: refused.
        let all: Vec<String> = (0..6).map(|n| format!("m{n}-1.jar")).collect();
        assert!(start(&i, &Start { keep: all, ..Default::default() }).is_err());
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn skips_try_another_split_and_two_mods_together_are_reported() {
        let i = inst("bisect skip");
        for n in 0..8 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let mut b = start(&i, &Start::default()).unwrap();
        let first = b.step.clone().unwrap().on;
        b = answer(&i, Answer::Skip).unwrap();
        assert_ne!(b.step.as_ref().unwrap().on, first, "a different split after a skip");
        assert_eq!(b.suspects.len(), 8);
        restore(&i).unwrap();

        // Broken only with both m1 and m6: no single mod is to blame.
        let b = run(&i, &Start::default(), |on| on.contains("m1-1.jar") && on.contains("m6-1.jar"));
        let done = b.done.unwrap();
        assert!(done.culprits.is_empty(), "{done:?}");
        assert!(done.note.contains("two mods together"));
        restore(&i).unwrap();

        // Always broken (it's not a mod): the last check says so.
        let b = run(&i, &Start::default(), |_| true);
        assert!(b.done.unwrap().culprits.is_empty());
        restore(&i).unwrap();
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn the_search_survives_a_restart() {
        let i = inst("bisect reload");
        for n in 0..5 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let before = on_now(&i);
        let b = start(&i, &Start { first: vec!["m3-1.jar".into()], ..Default::default() }).unwrap();
        assert_eq!(load(&i).unwrap().as_ref(), Some(&b));
        let b = answer(&i, Answer::Broken).unwrap();
        assert_eq!(load(&i).unwrap().as_ref(), Some(&b), "every answer is saved");
        assert!(start(&i, &Start::default()).is_err(), "one at a time");
        restore(&i).unwrap();
        assert_eq!(on_now(&i), before);
        assert!(load(&i).unwrap().is_none());
        std::fs::remove_dir_all(i.dir()).unwrap();
    }
}
