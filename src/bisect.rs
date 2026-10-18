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
//! - Every change of arrangement is saved before any file is renamed (`applied` false),
//!   then made ([`publish`], idempotent), then marked done. A switch cut short is finished
//!   by [`recover`] (before every launch) or by trying again; answers wait for it.
//! - The record is only deleted once every original file has been found and put back.
//!   A file renamed since is found by its SHA-1; one that can't be found keeps the record.
//! - Every operation holds `.bisect.lock` in the instance folder, so two never interleave.
//!
//! Mods are named by file name as if enabled (`sodium-0.6.0.jar`), since nothing
//! else changes the mods folder while a bisect runs (the mods tab is locked).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instance::{Instance, ModFile};
use crate::{content_meta, fsutil, mod_sets, net, world_sets};

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
    /// Each original file's SHA-1, to find it again if it's renamed meanwhile.
    #[serde(default)]
    pub ids: BTreeMap<String, String>,
    /// The mods are known to be as [`Bisect::wanted`] says. False from the moment a new
    /// arrangement is saved until every file has been switched to it.
    #[serde(default = "yes")]
    pub applied: bool,
    /// Ending: the mods on once it's over (as they were, or with the culprits off).
    #[serde(default)]
    pub ending: Option<BTreeSet<String>>,
    /// Originals the last try at ending couldn't find in the mods folder.
    #[serde(default)]
    pub unresolved: Vec<String>,
}

fn yes() -> bool {
    true
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
    match std::fs::File::open(&p).and_then(fsutil::read_capped) {
        Ok(bytes) => Ok(Some(toml::from_str(&String::from_utf8_lossy(&bytes)).with_context(|| format!("reading {}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save(inst: &Instance, b: &Bisect) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(b)?;
    fsutil::write_atomic(&path(inst), text.as_bytes(), 0o644).context("saving the search")?;
    Ok(())
}

/// Held through every operation on `inst`'s search, by any process, so a stop can't
/// interleave with an answer's renames.
fn lock(inst: &Instance) -> anyhow::Result<fsutil::FileLock> {
    fsutil::lock(&inst.dir().join(".bisect.lock")).context("locking the search")
}

/// The mod ids a jar answers to, and the ones it requires: its own and those of the jars
/// nested in it (Fabric's and Quilt's `jars`, Forge's jarjar), as deep and as many as
/// `world_sets::mod_inventory` follows, all counted for the file itself. Platform ids
/// (minecraft, the loader, java) come back too; nothing provides them, so they lead nowhere.
pub fn jar_links(jar: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut provides, mut requires) = (BTreeSet::new(), BTreeSet::new());
    if let Ok(mut zip) = std::fs::File::open(jar).map_err(anyhow::Error::from).and_then(|f| Ok(zip::ZipArchive::new(f)?)) {
        let mut budget = world_sets::NEST_JARS;
        links(&mut zip, 0, &mut budget, &mut provides, &mut requires);
    }
    // What the file brings along itself isn't needed from elsewhere.
    for id in &provides {
        requires.remove(id);
    }
    (provides, requires)
}

fn links<R: std::io::Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, depth: usize, budget: &mut usize, provides: &mut BTreeSet<String>, requires: &mut BTreeSet<String>) {
    use serde_json::Value;
    let json = |zip: &mut zip::ZipArchive<R>, name| content_meta::zip_bytes(zip, name).and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let strings = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|x| x.as_str().or(x["id"].as_str())).map(String::from).collect() };
    let id = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
    // A nested jar is a string (Quilt) or an object's "file" (Fabric) or "path" (jarjar).
    let paths = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|j| j.as_str().or(j["file"].as_str()).or(j["path"].as_str())).map(String::from).collect() };
    let mut nested = Vec::new();
    if let Some(f) = json(zip, "fabric.mod.json") {
        provides.extend(id(&f["id"]));
        provides.extend(strings(&f["provides"]));
        requires.extend(f["depends"].as_object().into_iter().flatten().map(|(k, _)| k.clone()));
        nested.extend(paths(&f["jars"]));
    } else if let Some(q) = json(zip, "quilt.mod.json") {
        let l = &q["quilt_loader"];
        provides.extend(id(&l["id"]));
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
        nested.extend(paths(&l["jars"]));
    } else if let Some(t) = ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"]
        .iter()
        .find_map(|n| content_meta::zip_bytes(zip, n).and_then(|b| toml::from_str::<toml::Value>(&String::from_utf8_lossy(&b)).ok()))
    {
        for m in t.get("mods").and_then(|m| m.as_array()).into_iter().flatten() {
            provides.extend(m.get("modId").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from));
        }
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
    } else if let Some(info) = json(zip, "mcmod.info") {
        let list = if info.is_array() { &info } else { &info["modList"] };
        for m in list.as_array().into_iter().flatten() {
            provides.extend(id(&m["modid"]));
            requires.extend(strings(&m["requiredMods"]).into_iter().map(|r| r.split('@').next().unwrap_or_default().to_string()));
        }
    }
    if let Some(j) = json(zip, "META-INF/jarjar/metadata.json") {
        nested.extend(paths(&j["jars"]));
    }
    if depth + 1 > world_sets::NEST_DEPTH {
        return;
    }
    nested.sort();
    nested.dedup();
    for path in nested {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let Some(bytes) = content_meta::zip_bytes(zip, &path) else { continue };
        if let Ok(mut inner) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
            links(&mut inner, depth + 1, budget, provides, requires);
        }
    }
}

/// Start finding the breaking mod in `inst`: every enabled mod not kept or known good
/// is a suspect. Turns the first step's mods on and the rest off, and saves the state.
/// Reads every enabled jar: run it off the UI thread.
pub fn start(inst: &Instance, opts: &Start) -> anyhow::Result<Bisect> {
    let _held = lock(inst)?;
    if load(inst)?.is_some() {
        bail!("it's already finding a breaking mod; stop that one first");
    }
    let files = mod_files(inst)?;
    let mut original: BTreeMap<String, bool> = BTreeMap::new();
    for f in &files {
        if original.insert(base(&f.name), f.enabled).is_some() {
            bail!("both {0} and {0}.disabled are in the mods folder; remove one of the two first", base(&f.name));
        }
    }
    let ids = files.iter().filter_map(|f| Some((base(&f.name), net::sha1_file(&f.path)?))).collect();
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
    // Kept mods stay on with everything they need: what they need is never a suspect,
    // since taking it away would take the kept mod with it.
    let keep: Vec<String> = opts.keep.iter().map(|k| base(k)).filter(|k| on.contains(k)).collect();
    let keep: BTreeSet<String> = closure(&needs, &keep);
    let good: BTreeSet<String> = opts.good.iter().map(|k| base(k)).collect();
    let mut suspects: Vec<String> = Vec::new();
    for f in opts.first.iter().map(|f| base(f)).chain(on.iter().cloned()) {
        if on.contains(&f) && !keep.contains(&f) && !good.contains(&f) && !suspects.contains(&f) {
            suspects.push(f);
        }
    }
    if suspects.is_empty() {
        bail!("there's nothing to test: every enabled mod is kept on, needed by one that is, or marked fine");
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
        ids,
        applied: false,
        ending: None,
        unresolved: Vec::new(),
    };
    b.step = Some(b.plan());
    // Saved before anything is renamed, so what it started from is never lost.
    save(inst, &b)?;
    publish(inst, &mut b)?;
    Ok(b)
}

/// `seed` and everything it requires, by `needs`.
fn closure<'a>(needs: &BTreeMap<String, BTreeSet<String>>, seed: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = seed.into_iter().cloned().collect();
    while let Some(f) = queue.pop_front() {
        if out.insert(f.clone()) {
            queue.extend(needs.get(&f).into_iter().flatten().filter(|d| !out.contains(*d)).cloned());
        }
    }
    out
}

impl Bisect {
    /// `seed` and everything it requires, among the mods on at the start.
    pub fn closure<'a>(&self, seed: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
        closure(&self.needs, seed)
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

    /// The mods that should be on now: the ending's, the step's, or (showing the result) the original ones.
    pub fn wanted(&self) -> BTreeSet<String> {
        match (&self.ending, &self.step) {
            (Some(on), _) => on.clone(),
            (None, Some(s)) => s.on.clone(),
            (None, None) => self.originally_on(),
        }
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
            (StepKind::Confirm, Answer::Works) if !self.closure(&self.keep).is_subset(&step.on) => {
                // The check took a kept mod away too (a search saved before kept mods brought
                // their needs along): it can't tell the suspect from the kept mod.
                self.done = Some(Done { culprits: culprits(self), confirmed: false, note: "This is the likely mod, but the last check also turned off a mod you kept on, so it couldn't confirm it.".into() });
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

/// The mods folder's files (an unreadable folder is an error, not an empty one).
fn mod_files(inst: &Instance) -> anyhow::Result<Vec<ModFile>> {
    let dir = inst.game_dir().join("mods");
    match std::fs::read_dir(&dir) {
        Ok(_) => Ok(inst.mods().into_iter().filter(|f| !f.is_dir).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// Each original file as it is now, by its name at the start; and the originals it can't
/// find. A file under another name (renamed by hand) counts if it's the only unclaimed
/// file with the original's SHA-1.
fn locate(inst: &Instance, b: &Bisect) -> anyhow::Result<(BTreeMap<String, ModFile>, Vec<String>)> {
    let mut found: BTreeMap<String, ModFile> = BTreeMap::new();
    let mut unclaimed = Vec::new();
    for f in mod_files(inst)? {
        let name = base(&f.name);
        if !b.original.contains_key(&name) {
            unclaimed.push(f);
        } else if found.insert(name.clone(), f).is_some() {
            bail!("both {name} and {name}.disabled are in the mods folder; remove one of the two first");
        }
    }
    let mut missing = Vec::new();
    let mut hashed: Option<Vec<(Option<String>, ModFile)>> = None;
    for name in b.original.keys().filter(|n| !found.contains_key(*n)).cloned().collect::<Vec<_>>() {
        let hit = b.ids.get(&name).and_then(|id| {
            let all = hashed.get_or_insert_with(|| unclaimed.iter().map(|f| (net::sha1_file(&f.path), f.clone())).collect());
            let claimed: BTreeSet<&Path> = found.values().map(|f| f.path.as_path()).collect();
            let mut hits = all.iter().filter(|(h, f)| h.as_ref() == Some(id) && !claimed.contains(f.path.as_path()));
            match (hits.next(), hits.next()) {
                (Some((_, f)), None) => Some(f.clone()),
                _ => None,
            }
        });
        match hit {
            Some(f) => {
                found.insert(name, f);
            }
            None => missing.push(name),
        }
    }
    Ok((found, missing))
}

/// Every original file on if in `on`, else off; files added since stay as they are. Fails
/// unless every original it found is then as asked. Returns the originals it couldn't find.
fn set_mods(inst: &Instance, b: &Bisect, on: &BTreeSet<String>) -> anyhow::Result<Vec<String>> {
    let (found, _) = locate(inst, b)?;
    let mut failed = Vec::new();
    for (name, f) in &found {
        if let Err(e) = mod_sets::rename(&f.path, on.contains(name)) {
            failed.push(format!("{e:#}"));
        }
    }
    if !failed.is_empty() {
        bail!("couldn't switch every mod: {}", failed.join("; "));
    }
    let (found, missing) = locate(inst, b)?;
    let wrong: Vec<&String> = found.iter().filter(|(n, f)| f.enabled != on.contains(*n)).map(|(n, _)| n).collect();
    if !wrong.is_empty() {
        bail!("couldn't switch every mod: {} didn't change", wrong.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok(missing)
}

/// Switch the mods to what `b` wants (saved beforehand, with `applied` false), then save
/// that they are. Safe to repeat. Returns the originals it couldn't find.
fn publish(inst: &Instance, b: &mut Bisect) -> anyhow::Result<Vec<String>> {
    let missing = set_mods(inst, b, &b.wanted()).context("the search is saved, so trying again finishes the switch")?;
    b.applied = true;
    save(inst, b)?;
    Ok(missing)
}

/// Whether the mods are as `b` wants right now (none of them switched by hand since).
fn in_place(inst: &Instance, b: &Bisect) -> anyhow::Result<bool> {
    let want = b.wanted();
    let (found, _) = locate(inst, b)?;
    Ok(found.iter().all(|(n, f)| f.enabled == want.contains(n)))
}

/// Answer the current step, then switch to the next one. Refused unless the mods are
/// exactly the step's (a switch cut short, or a mod switched since, is put right first
/// with [`recover`]). The next step is saved before any mod is switched.
pub fn answer(inst: &Instance, a: Answer) -> anyhow::Result<Bisect> {
    let _held = lock(inst)?;
    let mut b = load(inst)?.context("there's no search for a breaking mod running")?;
    if b.ending.is_some() {
        bail!("the search is ending; put the mods back first");
    }
    if !b.applied || !in_place(inst, &b)? {
        bail!("the mods aren't as this step set them: switch them again, then play the step");
    }
    b.answer(a)?;
    if b.done.is_some() {
        // Back to how it was while the result is read; the player picks what to keep.
        describe(inst, &mut b);
    }
    b.applied = false;
    save(inst, &b)?;
    publish(inst, &mut b)?;
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

/// End with the mods `on`: saved as the ending first, then switched, and the record is
/// deleted only once every original file is found and as it should be.
fn end(inst: &Instance, mut b: Bisect, on: BTreeSet<String>) -> anyhow::Result<()> {
    b.ending = Some(on);
    b.applied = false;
    save(inst, &b)?;
    finish(inst, &mut b)
}

fn finish(inst: &Instance, b: &mut Bisect) -> anyhow::Result<()> {
    let missing = publish(inst, b)?;
    if !missing.is_empty() {
        b.unresolved = missing.clone();
        save(inst, b)?;
        let (them, were) = if missing.len() == 1 { ("it", "it was") } else { ("them", "they were") };
        bail!(
            "couldn't find {} in the mods folder, so the search is kept to remember how {were}: put {them} back and try again, or forget the search",
            missing.join(", ")
        );
    }
    std::fs::remove_file(path(inst)).context("forgetting the search")?;
    Ok(())
}

/// Put the mods back exactly as they were when it started and forget the bisect.
pub fn restore(inst: &Instance) -> anyhow::Result<()> {
    let _held = lock(inst)?;
    let Some(b) = load(inst)? else { return Ok(()) };
    let on = b.originally_on();
    end(inst, b, on)
}

/// Restore, then turn the culprits off, with whatever needs them. Returns the mods
/// turned off besides the culprits.
pub fn disable_culprits(inst: &Instance) -> anyhow::Result<Vec<String>> {
    let _held = lock(inst)?;
    let b = load(inst)?.context("there's no search for a breaking mod running")?;
    let culprits: BTreeSet<String> = b.done.iter().flat_map(|d| &d.culprits).map(|c| c.file.clone()).collect();
    let also = b.dependents(&culprits);
    let on: BTreeSet<String> = b.originally_on().into_iter().filter(|f| !culprits.contains(f) && !also.contains(f)).collect();
    end(inst, b, on)?;
    Ok(also.into_iter().collect())
}

/// Put `inst`'s mods as its search says they should be: finish a switch cut short (or
/// undo one made by hand), or an ending. Run before every launch: an error stops the game.
/// An ending that can't find some originals keeps its record but doesn't stop the game.
pub fn recover(inst: &Instance) -> anyhow::Result<()> {
    let _held = lock(inst)?;
    let Some(mut b) = load(inst)? else { return Ok(()) };
    if b.ending.is_some() {
        b.applied = false;
        return match finish(inst, &mut b) {
            Err(_) if b.applied => Ok(()),
            r => r,
        };
    }
    if !b.applied || !in_place(inst, &b)? {
        b.applied = false;
        save(inst, &b)?;
        publish(inst, &mut b)?;
    }
    Ok(())
}

/// Forget the search without touching the mods (when its originals can't all be found).
pub fn forget(inst: &Instance) -> anyhow::Result<()> {
    let _held = lock(inst)?;
    match std::fs::remove_file(path(inst)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context("forgetting the search"),
        _ => Ok(()),
    }
}

/// Change the saved search with `f` (under its lock), if one is still there.
pub fn edit(inst: &Instance, f: impl FnOnce(&mut Bisect)) -> anyhow::Result<()> {
    let _held = lock(inst)?;
    if let Some(mut b) = load(inst)? {
        f(&mut b);
        save(inst, &b)?;
    }
    Ok(())
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

    fn chmod(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mods_of(i: &Instance) -> Vec<(String, bool)> {
        i.mods().into_iter().map(|m| (m.name, m.enabled)).collect()
    }

    #[test]
    fn a_search_is_saved_before_any_mod_is_switched() {
        let i = inst("bisect save first");
        for n in 0..4 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let before = mods_of(&i);
        // The instance folder can't be written: no search can be saved, so nothing is switched.
        chmod(&i.dir(), 0o555);
        assert!(start(&i, &Start::default()).is_err());
        chmod(&i.dir(), 0o755);
        assert_eq!(mods_of(&i), before, "nothing switched without a record");
        assert!(load(&i).unwrap().is_none());

        // The mods can't be switched: the search is kept, waiting, and launches put it right.
        let mods = i.game_dir().join("mods");
        chmod(&mods, 0o555);
        assert!(start(&i, &Start::default()).is_err());
        let b = load(&i).unwrap().expect("the search is kept");
        assert!(!b.applied && b.step.is_some());
        assert!(answer(&i, Answer::Works).is_err(), "no answer for a step that never ran");
        assert!(recover(&i).is_err(), "a launch is refused while the mods can't switch");
        chmod(&mods, 0o755);
        recover(&i).unwrap();
        let b = load(&i).unwrap().unwrap();
        assert!(b.applied);
        assert_eq!(on_now(&i), b.step.unwrap().on);
        restore(&i).unwrap();
        assert_eq!(mods_of(&i), before);
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn a_switch_cut_short_at_any_point_is_finished() {
        let i = inst("bisect interrupted");
        for n in 0..6 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let before = mods_of(&i);
        start(&i, &Start::default()).unwrap();
        let first = load(&i).unwrap().unwrap();
        // What an answer saves before switching: the next step, not yet applied.
        let mut next = first.clone();
        next.answer(Answer::Broken).unwrap();
        next.applied = false;
        let want = next.wanted();
        let flips: Vec<String> = first.original.keys().filter(|f| first.wanted().contains(*f) != want.contains(*f)).cloned().collect();
        assert!(!flips.is_empty());
        for k in 0..=flips.len() {
            // Back to step one, then the next saved and `k` of its renames made: a crash there.
            save(&i, &first).unwrap();
            set_mods(&i, &first, &first.wanted()).unwrap();
            save(&i, &next).unwrap();
            for f in &flips[..k] {
                let p = i.game_dir().join("mods").join(f);
                let p = if p.exists() { p } else { p.with_file_name(format!("{f}.disabled")) };
                mod_sets::rename(&p, want.contains(f)).unwrap();
            }
            assert!(answer(&i, Answer::Works).is_err(), "no answer until the switch is finished ({k})");
            recover(&i).unwrap();
            let b = load(&i).unwrap().unwrap();
            assert!(b.applied && b.history.len() == 1, "the saved step is the one now in place");
            assert_eq!(on_now(&i), want, "after {k} of {} renames", flips.len());
        }
        // A mod switched by hand during a step: answers wait until it's put right.
        let one = i.game_dir().join("mods").join(want.iter().next().unwrap());
        mod_sets::rename(&one, false).unwrap();
        assert!(answer(&i, Answer::Works).is_err());
        recover(&i).unwrap();
        answer(&i, Answer::Works).unwrap();
        // An ending cut short: the next launch finishes it.
        let mut b = load(&i).unwrap().unwrap();
        b.ending = Some(b.originally_on());
        b.applied = false;
        save(&i, &b).unwrap();
        recover(&i).unwrap();
        assert!(load(&i).unwrap().is_none());
        assert_eq!(mods_of(&i), before);
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn an_original_renamed_by_hand_is_found_by_its_bytes() {
        let i = inst("bisect renamed");
        for n in 0..4 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let b = start(&i, &Start::default()).unwrap();
        let off = b.original.keys().find(|f| !b.step.as_ref().unwrap().on.contains(*f)).unwrap().clone();
        let mods = i.game_dir().join("mods");
        std::fs::rename(mods.join(format!("{off}.disabled")), mods.join("renamed.jar.disabled")).unwrap();
        restore(&i).unwrap();
        assert!(load(&i).unwrap().is_none());
        assert!(mods.join("renamed.jar").is_file(), "on again, as it was before the search");
        assert_eq!(on_now(&i).len(), 4);
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn an_original_that_cant_be_found_keeps_the_search() {
        let i = inst("bisect missing");
        for n in 0..4 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let b = start(&i, &Start::default()).unwrap();
        let off = b.original.keys().find(|f| !b.step.as_ref().unwrap().on.contains(*f)).unwrap().clone();
        let mods = i.game_dir().join("mods");
        // Replaced under a new name by another tool: other bytes, so it can't be told it's the same.
        std::fs::remove_file(mods.join(format!("{off}.disabled"))).unwrap();
        jar(&i, "other-2.jar.disabled", Some("other"), &["x"]);
        let e = restore(&i).unwrap_err();
        assert!(format!("{e:#}").contains(&off), "{e:#}");
        let b = load(&i).unwrap().expect("kept, so nothing is forgotten");
        assert_eq!(b.unresolved, [off.clone()]);
        assert_eq!(on_now(&i).len(), 3, "the rest are back");
        assert!(mods.join("other-2.jar.disabled").exists(), "files it doesn't know stay as they are");
        recover(&i).unwrap();
        assert!(load(&i).unwrap().is_some(), "launches go ahead, the record stays");
        // Put back by hand: the next try ends it.
        jar(&i, &format!("{off}.disabled"), Some("whatever"), &[]);
        restore(&i).unwrap();
        assert!(load(&i).unwrap().is_none());
        assert!(mods.join(&off).is_file());
        // Or forgotten on purpose.
        start(&i, &Start::default()).unwrap();
        forget(&i).unwrap();
        assert!(load(&i).unwrap().is_none());
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn twins_are_refused_at_the_start() {
        let i = inst("bisect twins");
        jar(&i, "a-1.jar", Some("a"), &[]);
        jar(&i, "a-1.jar.disabled", Some("a"), &[]);
        jar(&i, "b-1.jar", Some("b"), &[]);
        let before = mods_of(&i);
        let e = start(&i, &Start::default()).unwrap_err();
        assert!(format!("{e:#}").contains("a-1.jar.disabled"), "{e:#}");
        assert!(load(&i).unwrap().is_none());
        assert_eq!(mods_of(&i), before);
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn a_stop_waits_for_an_answer_being_switched() {
        let i = inst("bisect serial");
        for n in 0..4 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let before = mods_of(&i);
        start(&i, &Start::default()).unwrap();
        let held = lock(&i).unwrap();
        let j = i.clone();
        let stop = std::thread::spawn(move || restore(&j));
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!stop.is_finished(), "waits while another operation has the search");
        drop(held);
        stop.join().unwrap().unwrap();
        assert!(answer(&i, Answer::Works).is_err(), "an answer after the stop finds nothing to answer");
        assert_eq!(mods_of(&i), before);
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    #[test]
    fn kept_mods_keep_what_they_need_through_the_confirming_step() {
        let i = inst("bisect kept needs");
        jar(&i, "a-1.jar", Some("a"), &["b"]);
        jar(&i, "b-1.jar", Some("b"), &[]);
        jar(&i, "c-1.jar", Some("c"), &[]);
        jar(&i, "d-1.jar", Some("d"), &[]);
        let keep = Start { keep: vec!["a-1.jar".into()], ..Default::default() };
        let b = start(&i, &keep).unwrap();
        assert!(!b.suspects.contains(&"b-1.jar".to_string()), "a kept mod's need isn't a suspect: {:?}", b.suspects);
        assert!(b.keep.contains("b-1.jar"));
        restore(&i).unwrap();
        // The kept mod is the broken one: every step has it and what it needs, and nothing is "confirmed".
        let b = run(&i, &keep, |on| {
            assert!(on.contains("a-1.jar") && on.contains("b-1.jar"), "the kept mod and its need are on in every step");
            on.contains("a-1.jar")
        });
        let done = b.done.unwrap();
        assert!(!done.culprits.iter().any(|c| c.file == "b-1.jar") && !done.confirmed, "{done:?}");
        restore(&i).unwrap();
        // With only the kept mod's need left to test, there's nothing to test.
        let only = Start { keep: vec!["a-1.jar".into()], good: vec!["c-1.jar".into(), "d-1.jar".into()], ..Default::default() };
        assert!(start(&i, &only).is_err());

        // An older record, with the need still a suspect: the check can't confirm it.
        let mut b = Bisect { suspects: vec!["b-1.jar".into()], keep: BTreeSet::from(["a-1.jar".into()]), ..start(&i, &Start::default()).unwrap() };
        restore(&i).unwrap();
        b.step = Some(b.plan());
        assert!(!b.step.as_ref().unwrap().on.contains("a-1.jar"), "the old plan took the kept mod away");
        b.answer(Answer::Works).unwrap();
        assert!(!b.done.unwrap().confirmed);
        std::fs::remove_dir_all(i.dir()).unwrap();
    }

    /// A Fabric jar `name` with id `id`, carrying a nested jar with id `inner` that requires `needs`.
    fn nesting_jar(i: &Instance, name: &str, id: &str, inner: &str, needs: &str) {
        use std::io::Write;
        let opts = zip::write::SimpleFileOptions::default();
        let mut nested = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        nested.start_file("fabric.mod.json", opts).unwrap();
        write!(nested, r#"{{"schemaVersion": 1, "id": "{inner}", "version": "1", "depends": {{"{needs}": "*", "{id}": "*"}}}}"#).unwrap();
        let nested = nested.finish().unwrap().into_inner();
        let dir = i.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(dir.join(name)).unwrap());
        z.start_file("fabric.mod.json", opts).unwrap();
        write!(z, r#"{{"schemaVersion": 1, "id": "{id}", "version": "1", "depends": {{"minecraft": "*"}}, "jars": [{{"file": "META-INF/jars/{inner}.jar"}}]}}"#).unwrap();
        z.start_file(format!("META-INF/jars/{inner}.jar"), opts).unwrap();
        z.write_all(&nested).unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn a_nested_mods_needs_count_for_its_jar() {
        let i = inst("bisect nested needs");
        nesting_jar(&i, "a-1.jar", "a", "nested", "zlib");
        jar(&i, "zlib-1.jar", Some("zlib"), &[]);
        for n in 0..4 {
            jar(&i, &format!("m{n}-1.jar"), Some(&format!("m{n}")), &[]);
        }
        let (provides, requires) = jar_links(&i.game_dir().join("mods/a-1.jar"));
        assert!(provides.contains("nested") && requires.contains("zlib"), "{provides:?} {requires:?}");
        assert!(!requires.contains("a"), "what the jar brings along isn't needed from elsewhere");

        let b = start(&i, &Start { first: vec!["a-1.jar".into()], ..Default::default() }).unwrap();
        assert_eq!(b.needs["a-1.jar"], BTreeSet::from(["zlib-1.jar".into()]));
        // Every step that has a on has zlib on too.
        let mut b = b;
        while b.done.is_none() {
            let on = &b.step.as_ref().unwrap().on;
            assert!(!on.contains("a-1.jar") || on.contains("zlib-1.jar"), "a without zlib: {on:?}");
            b = answer(&i, if on.contains("m2-1.jar") { Answer::Broken } else { Answer::Works }).unwrap();
        }
        restore(&i).unwrap();

        // The sets checklist ticks zlib along with a.
        let entries = mod_sets::scan(&i);
        let needs = mod_sets::needs(&entries);
        assert!(mod_sets::with_needs(&["a".into()], &needs).contains("zlib"));
        std::fs::remove_dir_all(i.dir()).unwrap();
    }
}
