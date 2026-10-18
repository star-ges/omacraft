//! Why the game crashed: reads a crash report or a game log and names the mods most
//! likely at fault, each with the lines that say so.
//!
//! - What the loaders and Minecraft say outright comes first: Forge's `-- MOD x --`
//!   blocks and loading errors, Fabric's and Quilt's unmet dependencies, failed mixins,
//!   failed entrypoints. These are **certain**.
//! - Minecraft's own "Suspected Mods", mixin handlers and mod frames near the top of a
//!   stack trace are **likely**; frames further down are **possible**. Stack frames are
//!   matched to jars by the packages each jar holds ([`ModIndex`]).
//! - Causes that aren't a mod (out of memory, the wrong Java, graphics drivers) are
//!   explained in plain words ([`Cause`]).
//!
//! Nothing here guesses past the evidence: a crash with no clear culprit says so.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::content_meta;
use crate::instance::{ContentKind, Instance};
use crate::loaders::LoaderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Possible,
    Likely,
    Certain,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Certain => "certain",
            Confidence::Likely => "likely",
            Confidence::Possible => "possible",
        }
    }
}

/// A line of the log that points somewhere: its index in `text.lines()`, and the line.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub line: usize,
    pub text: String,
}

/// A mod the crash points at.
#[derive(Debug, Clone)]
pub struct Suspect {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    /// Its jar in the instance, as it was when analysed (may be `.disabled`).
    pub file: Option<PathBuf>,
    pub sha1: Option<String>,
    pub confidence: Confidence,
    /// One line, for people: "its mixin into LevelRenderer failed".
    pub reason: String,
    pub evidence: Vec<Evidence>,
}

impl Suspect {
    /// "Sodium 0.5.8".
    pub fn label(&self) -> String {
        match &self.version {
            Some(v) if !v.is_empty() => format!("{} {v}", self.name),
            _ => self.name.clone(),
        }
    }
}

/// A cause that isn't a mod's fault.
#[derive(Debug, Clone)]
pub struct Cause {
    /// Short: "out of memory".
    pub title: String,
    /// What to do about it, in a sentence or two.
    pub advice: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Default)]
pub struct Analysis {
    /// Whether the text shows a crash at all (a crash report always does).
    pub crashed: bool,
    /// The crash report's own one-liner ("Description: …").
    pub description: Option<String>,
    /// Most likely first.
    pub suspects: Vec<Suspect>,
    pub causes: Vec<Cause>,
}

impl Analysis {
    /// The mod to blame, if one stands out (for the bisect to try first).
    #[allow(dead_code)]
    pub fn culprit(&self) -> Option<&Suspect> {
        self.suspects.first().filter(|s| s.confidence >= Confidence::Likely)
    }

    /// One line for the top of the panel: "likely cause: Sodium 0.5.8".
    pub fn headline(&self) -> String {
        let certain: Vec<String> = self.suspects.iter().filter(|s| s.confidence == Confidence::Certain).map(Suspect::label).collect();
        match (self.suspects.first(), self.causes.first()) {
            _ if certain.len() == 2 => format!("causes: {} and {}", certain[0], certain[1]),
            _ if certain.len() > 2 => format!("causes: {}, {} and {} more", certain[0], certain[1], certain.len() - 2),
            (Some(s), _) if s.confidence == Confidence::Certain => format!("cause: {}", s.label()),
            (_, Some(c)) => c.title.clone(),
            (Some(s), None) if s.confidence == Confidence::Likely => format!("likely cause: {}", s.label()),
            _ if self.crashed => "no single mod stands out".into(),
            _ => "no crash found".into(),
        }
    }

    /// Plain text for the clipboard (a bug report, a message to a friend).
    pub fn summary(&self) -> String {
        let mut out = vec![format!("Omacraft crash analysis: {}", self.headline())];
        if let Some(d) = &self.description {
            out.push(format!("Minecraft says: {d}"));
        }
        for c in &self.causes {
            out.push(format!("\n{}: {}", c.title, c.advice));
            out.extend(c.evidence.iter().map(|e| format!("  line {}: {}", e.line + 1, e.text)));
        }
        for s in &self.suspects {
            out.push(format!("\n[{}] {} ({}): {}", s.confidence.label(), s.label(), s.id, s.reason));
            out.extend(s.evidence.iter().map(|e| format!("  line {}: {}", e.line + 1, e.text)));
        }
        if self.suspects.is_empty() && self.causes.is_empty() {
            out.push("No mod or other cause stands out in this log.".into());
        }
        out.join("\n")
    }
}

// -- what the instance's jars hold ------------------------------------------------

/// Which loader a jar is written for, from the metadata it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JarKind {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    /// Forge 1.6–1.12's `mcmod.info`.
    LegacyForge,
    #[default]
    Unknown,
}

/// What a crash analysis needs to know about one jar.
#[derive(Debug, Clone, Default)]
pub struct JarFacts {
    pub id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub sha1: Option<String>,
    /// Every mod id the jar declares, nested ones included.
    pub ids: Vec<String>,
    pub kinds: Vec<JarKind>,
    /// Java packages its classes are in (dotted), nested jars' included.
    pub packages: Vec<String>,
    /// Mixin config files at its root ("sodium.mixins.json").
    pub mixin_configs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModEntry {
    pub file: PathBuf,
    pub enabled: bool,
    pub facts: Arc<JarFacts>,
}

impl ModEntry {
    fn name(&self) -> String {
        self.facts.name.clone().or(self.facts.id.clone()).unwrap_or_else(|| file_stem(&self.file))
    }
}

fn file_stem(p: &Path) -> String {
    let n = p.file_name().unwrap_or_default().to_string_lossy();
    n.trim_end_matches(".disabled").trim_end_matches(".jar").to_string()
}

/// The instance's mods, looked up by id, by package and by mixin config.
#[derive(Debug, Default)]
pub struct ModIndex {
    pub mods: Vec<ModEntry>,
    by_id: HashMap<String, usize>,
    by_package: HashMap<String, usize>,
    by_mixin: HashMap<String, usize>,
}

/// Jar facts by path, kept while the jar's size and mtime stay the same.
type FactsCache = HashMap<PathBuf, (u64, Option<SystemTime>, Arc<JarFacts>)>;
static FACTS: Mutex<Option<FactsCache>> = Mutex::new(None);

/// Most nested jars a jar's packages are read from, and how deep.
const NESTED_JARS: usize = 64;

impl ModIndex {
    /// Read every mod jar of `inst` (enabled or not). Hashes and opens each jar once
    /// per change, so run it off the UI thread.
    pub fn of(inst: &Instance) -> Self {
        Self::new(inst.mods().into_iter().filter(|m| !m.is_dir).map(|m| ModEntry { facts: facts(&m.path), file: m.path, enabled: m.enabled }).collect())
    }

    pub fn new(mods: Vec<ModEntry>) -> Self {
        let mut ix = ModIndex::default();
        // Enabled jars win any clash: they're the ones the game loaded.
        let mut order: Vec<usize> = (0..mods.len()).collect();
        order.sort_by_key(|&i| !mods[i].enabled);
        for &i in &order {
            let f = &mods[i].facts;
            for id in f.id.iter().chain(&f.ids) {
                ix.by_id.entry(id.to_lowercase()).or_insert(i);
            }
            for p in &f.packages {
                ix.by_package.entry(p.clone()).or_insert(i);
            }
            for c in &f.mixin_configs {
                ix.by_mixin.entry(c.to_lowercase()).or_insert(i);
            }
        }
        ix.mods = mods;
        ix
    }

    pub fn by_id(&self, id: &str) -> Option<&ModEntry> {
        self.by_id.get(&id.to_lowercase()).map(|&i| &self.mods[i])
    }

    /// The jar holding class `class` (dotted, `pkg.Class` or `pkg.Class$Inner`).
    pub fn by_class(&self, class: &str) -> Option<&ModEntry> {
        let mut pkg = class.rsplit_once('.')?.0;
        if is_library(pkg) {
            return None;
        }
        // A package no jar holds classes in directly falls back to its parent, down to
        // three parts ("me.jellysquid.mods" is shared by several mods).
        loop {
            if let Some(&i) = self.by_package.get(pkg) {
                return Some(&self.mods[i]);
            }
            match pkg.rsplit_once('.') {
                Some((parent, _)) if parent.matches('.').count() >= 2 => pkg = parent,
                _ => return None,
            }
        }
    }

    pub fn by_mixin_config(&self, config: &str) -> Option<&ModEntry> {
        self.by_mixin.get(&config.to_lowercase()).map(|&i| &self.mods[i])
    }
}

/// What a jar holds, from the cache if it hasn't changed.
pub fn facts(path: &Path) -> Arc<JarFacts> {
    let meta = std::fs::metadata(path).ok();
    let (size, mtime) = (meta.as_ref().map_or(0, |m| m.len()), meta.and_then(|m| m.modified().ok()));
    let mut cache = FACTS.lock().unwrap();
    if let Some((s, m, f)) = cache.get_or_insert_with(HashMap::new).get(path)
        && *s == size
        && *m == mtime
    {
        return f.clone();
    }
    drop(cache);
    let f = Arc::new(read_facts(path));
    FACTS.lock().unwrap().get_or_insert_with(HashMap::new).insert(path.to_path_buf(), (size, mtime, f.clone()));
    f
}

fn read_facts(path: &Path) -> JarFacts {
    let meta = content_meta::read_local(path, false, ContentKind::Mods);
    let mut f = JarFacts {
        id: meta.mod_id,
        name: meta.name,
        version: meta.version,
        sha1: meta.sha1,
        ids: crate::world_sets::mod_inventory(path).into_keys().collect(),
        ..Default::default()
    };
    let Ok(mut zip) = std::fs::File::open(path).map_err(anyhow::Error::from).and_then(|file| Ok(zip::ZipArchive::new(file)?)) else {
        return f;
    };
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    for (file, kind) in [
        ("fabric.mod.json", JarKind::Fabric),
        ("quilt.mod.json", JarKind::Quilt),
        ("META-INF/neoforge.mods.toml", JarKind::NeoForge),
        ("META-INF/mods.toml", JarKind::Forge),
        ("mcmod.info", JarKind::LegacyForge),
    ] {
        if has(file) {
            f.kinds.push(kind);
        }
    }
    let mut packages = std::collections::BTreeSet::new();
    add_packages(&names, &mut packages);
    f.mixin_configs = names.iter().filter(|n| !n.contains('/') && n.ends_with(".json") && n.to_lowercase().contains("mixin")).cloned().collect();
    // Bundled jars (Fabric's `jars`, Forge's jarjar): their classes are this mod's.
    let nested: Vec<String> = names.iter().filter(|n| n.ends_with(".jar")).take(NESTED_JARS).cloned().collect();
    for n in nested {
        let Some(bytes) = content_meta::zip_bytes(&mut zip, &n) else { continue };
        if let Ok(inner) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
            let inner_names: Vec<String> = inner.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
            add_packages(&inner_names, &mut packages);
        }
    }
    f.packages = packages.into_iter().collect();
    f
}

fn add_packages(names: &[String], out: &mut std::collections::BTreeSet<String>) {
    for n in names {
        if let Some(class) = n.strip_suffix(".class")
            && !n.starts_with("META-INF/")
            && let Some((dir, _)) = class.rsplit_once('/')
        {
            out.insert(dir.replace('/', "."));
        }
    }
}

/// Packages that belong to Minecraft, a loader, Java or a common library: a frame in
/// one of these doesn't point at any mod (even one that bundles a copy).
const LIBRARIES: &[&str] = &[
    "java.", "javax.", "jdk.", "sun.", "com.sun.", "net.minecraft.", "com.mojang.", "net.fabricmc.loader.", "net.fabricmc.api.",
    "org.quiltmc.loader.", "cpw.mods.", "net.minecraftforge.", "net.neoforged.", "org.spongepowered.", "org.lwjgl.", "com.google.",
    "org.apache.", "io.netty.", "it.unimi.", "org.slf4j.", "kotlin.", "kotlinx.", "org.objectweb.", "com.llamalad7.mixinextras.",
    "oshi.", "org.joml.", "com.electronwill.", "org.jetbrains.", "net.java.", "com.ibm.icu.", "scala.", "org.openjdk.", "com.github.benmanes.",
];

fn is_library(pkg: &str) -> bool {
    let p = format!("{pkg}.");
    LIBRARIES.iter().any(|l| p.starts_with(l))
}

/// Mod ids Forge's module frames name that aren't mods: Minecraft and the loaders.
const NOT_MODS: &[&str] = &["minecraft", "forge", "neoforge", "fml", "fml_loader", "fmlcore", "javafmllanguage", "lowcodelanguage", "mclanguage", "mixin", "mixinextras", "java", "fabricloader", "quilt_loader"];

// -- reading the log ----------------------------------------------------------------

/// What the game was: for advice that depends on it.
#[derive(Debug, Clone, Copy)]
pub struct Game {
    pub loader: LoaderKind,
    pub memory_mb: u32,
}

impl Game {
    pub fn of(inst: &Instance, default_memory_mb: u32) -> Self {
        Game { loader: inst.loader.kind(), memory_mb: inst.memory_mb.unwrap_or(default_memory_mb) }
    }
}

/// Analyse `inst`'s crash report or log text. Reads the jars: run it off the UI thread.
pub fn analyze(inst: &Instance, default_memory_mb: u32, text: &str) -> Analysis {
    analyze_with(text, &ModIndex::of(inst), Game::of(inst, default_memory_mb))
}

/// A suspect as it's found, before the hits for one mod are merged.
struct Hit {
    /// The mod id as the log gives it (lowercased).
    id: String,
    /// The name the log gives it, if any.
    name: Option<String>,
    version: Option<String>,
    confidence: Confidence,
    reason: String,
    line: usize,
}

struct Reader<'a> {
    lines: Vec<&'a str>,
    hits: Vec<Hit>,
    causes: Vec<Cause>,
    ix: &'a ModIndex,
}

/// Lines longer than this are cut in evidence.
const EVIDENCE_CHARS: usize = 220;
/// Evidence kept per suspect or cause.
const EVIDENCE_LINES: usize = 6;

fn evidence(lines: &[&str], line: usize) -> Evidence {
    let t = lines[line].trim();
    let text = if t.chars().count() > EVIDENCE_CHARS { format!("{}…", t.chars().take(EVIDENCE_CHARS).collect::<String>()) } else { t.to_string() };
    Evidence { line, text }
}

/// `s` between `start` and the next `end` after it.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = s.find(start)? + start.len();
    let to = s[from..].find(end)? + from;
    Some(&s[from..to])
}

fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// "net.minecraft.client.renderer.LevelRenderer" → "LevelRenderer"; "net/minecraft/class_761" → "class_761".
fn short_class(c: &str) -> &str {
    c.rsplit(['.', '/']).next().unwrap_or(c).split('$').next().unwrap_or(c)
}

/// Markers that a log shows a crash (a crash report always does).
const CRASH_MARKERS: &[&str] = &[
    "---- Minecraft Crash Report ----",
    "#@!@# Game crashed!",
    "Game crashed!",
    "Exception in thread \"main\"",
    "Exception in thread \"Render thread\"",
    "FormattedException",
    "Incompatible mods found!",
    "incompatible mod set",
    "Mod resolution failed",
    "ModLoadingException",
    "LoadingFailedException",
    "OutOfMemoryError",
    "UnsupportedClassVersionError",
    "A fatal error has been detected by the Java Runtime Environment",
    "Failed to create mod instance",
    "Could not execute entrypoint stage",
    "has failed to load correctly",
    "Missing or unsupported mandatory dependencies",
];

/// Whether `text` shows a crash, cheaply (no jars read): worth a full [`analyze`].
pub fn looks_crashed(text: &str) -> bool {
    CRASH_MARKERS.iter().any(|m| text.contains(m))
}

/// Analyse `text` against the mods in `ix`.
pub fn analyze_with(text: &str, ix: &ModIndex, game: Game) -> Analysis {
    let mut r = Reader { lines: text.lines().collect(), hits: Vec::new(), causes: Vec::new(), ix };
    let crashed = looks_crashed(text);
    let description = r.lines.iter().find_map(|l| l.strip_prefix("Description: ")).map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
    for i in 0..r.lines.len() {
        r.forge_mod_block(i);
        r.forge_loading(i);
        r.dependencies(i);
        r.mixins(i);
        r.entrypoint(i);
        r.suspected(i);
        r.non_mod(i, game);
    }
    r.stack_frames();
    if crashed || !r.hits.is_empty() || !r.causes.is_empty() {
        r.wrong_loader(game);
    }
    let crashed = crashed || r.hits.iter().any(|h| h.confidence == Confidence::Certain) || !r.causes.is_empty();
    let suspects = r.merge();
    Analysis { crashed, description, suspects, causes: r.causes }
}

impl Reader<'_> {
    fn hit(&mut self, id: &str, name: Option<&str>, version: Option<&str>, confidence: Confidence, reason: impl Into<String>, line: usize) {
        let id = id.trim().to_lowercase();
        if id.is_empty() || NOT_MODS.contains(&id.as_str()) {
            return;
        }
        self.hits.push(Hit { id, name: name.map(String::from), version: version.map(String::from), confidence, reason: reason.into(), line });
    }

    /// The mod's display name for a reason, from the jar or else the log.
    fn display(&self, id: &str, fallback: Option<&str>) -> String {
        self.ix.by_id(id).map(|m| m.name()).or(fallback.map(String::from)).unwrap_or_else(|| id.to_string())
    }

    fn cause(&mut self, title: &str, advice: String, line: usize) {
        let e = evidence(&self.lines, line);
        match self.causes.iter_mut().find(|c| c.title == title) {
            Some(c) if c.evidence.len() < EVIDENCE_LINES && !c.evidence.iter().any(|x| x.text == e.text) => c.evidence.push(e),
            Some(_) => {}
            None => self.causes.push(Cause { title: title.into(), advice, evidence: vec![e] }),
        }
    }

    /// Forge's crash report block for a mod that failed to load:
    /// `-- MOD examplemod --` … `Failure message: …` + the exception under it.
    fn forge_mod_block(&mut self, i: usize) {
        let Some(id) = self.lines[i].trim().strip_prefix("-- MOD ").and_then(|s| s.strip_suffix(" --")) else { return };
        let id = id.trim();
        let mut failure = None;
        let mut version = None;
        for (j, l) in self.lines.iter().enumerate().skip(i + 1).take(40) {
            let t = l.trim();
            if t.starts_with("-- ") {
                break;
            }
            if let Some(m) = t.strip_prefix("Failure message:") {
                // The exception sits on the line under it.
                let next = self.lines.get(j + 1).map(|n| n.trim()).filter(|n| n.contains("Exception") || n.contains("Error"));
                failure = Some((j, m.trim().to_string(), next.map(String::from)));
            }
            if let Some(v) = t.strip_prefix("Mod Version:") {
                version = Some(v.trim().to_string());
            }
        }
        if let Some((line, msg, exception)) = failure {
            let reason = match exception.as_deref().map(short_exception) {
                Some(e) if !e.is_empty() => format!("failed while loading ({e})"),
                _ if !msg.is_empty() && msg != "NONE" => format!("failed while loading: {}", msg.trim_end_matches('.')),
                _ => "failed while loading".into(),
            };
            self.hit(id, None, version.as_deref(), Confidence::Certain, reason, line);
        }
    }

    /// Forge and NeoForge loading errors in the log.
    fn forge_loading(&mut self, i: usize) {
        let l = self.lines[i];
        if let Some(rest) = l.split("Failed to create mod instance. ModID: ").nth(1) {
            let id = rest.split([',', ' ']).next().unwrap_or_default();
            self.hit(id, None, None, Confidence::Certain, "couldn't be started by the loader", i);
        }
        // "Sodium (sodium) has failed to load correctly",
        // "Create (create) encountered an error during the common_setup event phase".
        for (marker, why) in [(") has failed to load correctly", None), (") encountered an error during the ", Some(" event phase"))] {
            if let Some(at) = l.find(marker)
                && let Some(open) = l[..at].rfind(" (")
            {
                let id = &l[open + 2..at];
                let name = l[..open].rsplit([':', ']']).next().unwrap_or_default().trim();
                let reason = match why.and_then(|end| between(&l[at..], marker, end)) {
                    Some(phase) => format!("failed during loading ({})", phase.replace('_', " ")),
                    None => "failed to load".into(),
                };
                if is_id(id) {
                    self.hit(id, Some(name).filter(|n| !n.is_empty()), None, Confidence::Certain, reason, i);
                }
            }
        }
    }

    /// Unmet dependencies, as Forge, NeoForge, Fabric and Quilt word them.
    fn dependencies(&mut self, i: usize) {
        let l = self.lines[i].trim();
        // Forge 1.17–1.20: Mod ID: 'create', Requested by: 'createaddition', Expected range: '[0.5.1,)', Actual version: '[MISSING]'
        if l.starts_with("Mod ID: '")
            && let (Some(dep), Some(by), Some(range), Some(actual)) = (between(l, "Mod ID: '", "'"), between(l, "Requested by: '", "'"), between(l, "Expected range: '", "'"), between(l, "Actual version: '", "'"))
        {
            let dep_name = self.display(dep, None);
            let reason = if actual == "[MISSING]" {
                format!("needs {dep_name} {range}, which isn't installed")
            } else if dep == "minecraft" {
                format!("is made for Minecraft {range}; this is {actual}")
            } else {
                format!("needs {dep_name} {range}, but {actual} is installed")
            };
            self.hit(by, None, None, Confidence::Certain, reason, i);
            return;
        }
        // NeoForge: "Mod Crafts & Additions (createaddition) requires create 0.5.1 or above"
        //           "Currently, create is not installed"
        if let Some(next) = self.lines.get(i + 1).map(|n| n.trim())
            && next.starts_with("Currently, ")
            && let Some(at) = l.find(") requires ")
            && let Some(open) = l[..at].rfind(" (")
        {
            let id = &l[open + 2..at];
            let name = l[..open].strip_prefix("Mod ").unwrap_or(&l[..open]).trim();
            let wants = &l[at + ") requires ".len()..];
            let dep = wants.split_whitespace().next().unwrap_or_default();
            let dep_name = self.display(dep, None);
            let spec = wants[dep.len()..].trim();
            let reason = if next.ends_with("is not installed") {
                format!("needs {dep_name} {spec}, which isn't installed")
            } else {
                format!("needs {dep_name} {spec}; {}", lower_first(next.trim_start_matches("Currently, ")))
            };
            if is_id(id) {
                self.hit(id, Some(name), None, Confidence::Certain, reason.replace("  ", " "), i);
            }
            return;
        }
        // Fabric and Quilt:
        // - Mod 'Iris' (iris) 1.7.0 requires version 0.5.8 or later of mod 'Sodium' (sodium), but only the wrong version is present: 0.4.0!
        // - Mod 'Mod Menu' (modmenu) 7.0.0 requires any version of fabric-api, which is missing!
        // - Mod 'X' (x) 1.0 is incompatible with any version of mod 'Y' (y), but it is present: 2.0!
        let Some(rest) = l.trim_start_matches("- ").strip_prefix("Mod '") else { return };
        let Some((name, rest)) = rest.split_once("' (") else { return };
        let Some((id, rest)) = rest.split_once(") ") else { return };
        let Some(sp) = rest.find(' ') else { return };
        let (version, rest) = (&rest[..sp], rest[sp + 1..].trim());
        let (breaks, rest) = if let Some(r) = rest.strip_prefix("requires ") {
            (false, r)
        } else if let Some(r) = rest.strip_prefix("is incompatible with ").or(rest.strip_prefix("breaks ")) {
            (true, r)
        } else {
            return;
        };
        let Some(of) = rest.find(" of ") else { return };
        let spec = rest[..of].trim_start_matches("version ").trim_start_matches("any version").trim();
        let after = &rest[of + 4..];
        let (dep_id, dep_name, tail) = if let Some(d) = after.strip_prefix("mod '") {
            let (dn, t) = d.split_once("' (").unwrap_or((d, ""));
            let (di, t) = t.split_once(')').unwrap_or((t, ""));
            (di.to_string(), Some(dn.to_string()), t)
        } else {
            let end = after.find([',', '!']).unwrap_or(after.len());
            (after[..end].trim().to_string(), None, &after[end..])
        };
        let dep_label = self.display(&dep_id, dep_name.as_deref());
        let present = tail.split(": ").nth(1).map(|v| v.trim_end_matches('!').trim().to_string());
        let spec_txt = if spec.is_empty() { String::new() } else { format!(" {spec}") };
        let reason = if breaks {
            format!("can't run alongside {dep_label}{}", present.as_ref().map(|v| format!(" {v}")).unwrap_or_default())
        } else if tail.contains("missing") {
            format!("needs {dep_label}{spec_txt}, which isn't installed")
        } else if dep_id == "minecraft" {
            format!("is made for Minecraft{spec_txt}; this is {}", present.clone().unwrap_or_default())
        } else if dep_id == "java" {
            format!("needs Java{spec_txt}")
        } else {
            format!("needs {dep_label}{spec_txt}, but {} is installed", present.clone().unwrap_or_else(|| "another version".into()))
        };
        self.hit(id, Some(name), Some(version), Confidence::Certain, reason, i);
        if breaks {
            self.hit(&dep_id, dep_name.as_deref(), present.as_deref(), Confidence::Likely, format!("{} can't run alongside it", name), i);
        }
    }

    fn mixins(&mut self, i: usize) {
        let l = self.lines[i];
        // Mixin apply for mod sodium failed sodium.mixins.json:core.MixinX from mod sodium -> net.minecraft.class_761: …
        if let Some(rest) = l.split("Mixin apply for mod ").nth(1) {
            let id = rest.split_whitespace().next().unwrap_or_default();
            let target = rest.split(" -> ").nth(1).and_then(|t| t.split([':', ' ']).next()).map(short_class);
            let reason = match target {
                Some(t) => format!("its mixin into {t} failed"),
                None => "its mixin failed".into(),
            };
            self.hit(id, None, None, Confidence::Certain, reason, i);
            return;
        }
        // [… -> sodium.mixins.json:MixinWorldRenderer from mod sodium -> Prepare Injections -> …]
        if (l.contains("InjectionError") || l.contains("injection failure") || l.contains("InvalidMixinException") || l.contains("MixinApplyError") || l.contains("InvalidInjectionException"))
            && let Some(id) = between(l, " from mod ", " ")
        {
            let mixin = l.split(" from mod ").next().and_then(|s| s.rsplit(':').next()).map(short_class).unwrap_or("");
            let reason = if mixin.is_empty() { "its mixin failed".to_string() } else { format!("its mixin {mixin} failed") };
            self.hit(id.trim_end_matches([']', ',']), None, None, Confidence::Certain, reason, i);
            return;
        }
        // Mixin [sodium.mixins.json:features.X] from phase [DEFAULT] in config [sodium.mixins.json] FAILED during APPLY
        if l.contains("FAILED during") && l.contains("Mixin [")
            && let Some(config) = between(l, "in config [", "]")
        {
            let mixin = between(l, "Mixin [", "]").and_then(|m| m.rsplit(':').next()).map(short_class).unwrap_or("");
            if let Some(m) = self.ix.by_mixin_config(config) {
                let id = m.facts.id.clone().unwrap_or_else(|| file_stem(&m.file));
                self.hit(&id, None, None, Confidence::Certain, format!("its mixin {mixin} failed"), i);
            } else {
                let id = config.split('.').next().unwrap_or(config).trim_start_matches("mixins.").to_string();
                self.hit(&id, None, None, Confidence::Likely, format!("its mixin {mixin} failed (from {config})"), i);
            }
        }
        // Forge's "Mixins in Stacktrace:" list: `pkg.MixinX (sodium.mixins.json)`.
        if l.trim() == "Mixins in Stacktrace:" {
            for j in i + 1..self.lines.len().min(i + 60) {
                let t = self.lines[j].trim();
                if t.is_empty() || !self.lines[j].starts_with(['\t', ' ']) {
                    break;
                }
                if let Some(config) = t.rsplit_once(" (").and_then(|(_, c)| c.strip_suffix(')'))
                    && let Some(m) = self.ix.by_mixin_config(config)
                {
                    let id = m.facts.id.clone().unwrap_or_else(|| file_stem(&m.file));
                    self.hit(&id, None, None, Confidence::Likely, "its mixins changed code in the crash's stack trace", j);
                }
            }
        }
    }

    /// Could not execute entrypoint stage 'client' due to errors, provided by 'examplemod' at 'com.example.Client'!
    fn entrypoint(&mut self, i: usize) {
        let l = self.lines[i];
        if let (Some(stage), Some(id)) = (between(l, "entrypoint stage '", "'"), between(l, "provided by '", "'")) {
            self.hit(id, None, None, Confidence::Certain, format!("crashed while starting up (its {stage} entrypoint)"), i);
        }
    }

    /// Minecraft's "Suspected Mod(s):" (1.18+): inline, or one per indented line after.
    fn suspected(&mut self, i: usize) {
        let l = self.lines[i].trim();
        let Some(rest) = l.strip_prefix("Suspected Mods:").or(l.strip_prefix("Suspected Mod:")) else { return };
        let mut entries: Vec<(usize, String)> = Vec::new();
        if !rest.trim().is_empty() {
            entries.push((i, rest.trim().to_string()));
        }
        for j in i + 1..self.lines.len().min(i + 30) {
            let raw = self.lines[j];
            let t = raw.trim();
            if t.is_empty() || !raw.starts_with(['\t', ' ']) || t.starts_with("Stacktrace") {
                break;
            }
            entries.push((j, t.to_string()));
        }
        for (j, e) in entries {
            if e.eq_ignore_ascii_case("none") || e.starts_with("at ") || e.starts_with("Issue tracker") {
                continue;
            }
            // "Sodium (sodium), Version: 0.5.8"
            if let Some((name, rest)) = e.split_once(" (")
                && let Some((id, rest)) = rest.split_once(')')
            {
                let version = rest.split("Version:").nth(1).map(|v| v.trim());
                self.hit(id, Some(name.trim()), version, Confidence::Likely, "Minecraft's crash report suspects it", j);
            }
        }
    }

    fn non_mod(&mut self, i: usize, game: Game) {
        let l = self.lines[i];
        if l.contains("java.lang.OutOfMemoryError") {
            self.cause(
                "out of memory",
                format!(
                    "Minecraft ran out of memory. Give it more under edit → memory (it has {} now); big modpacks often want 6–8 GB.",
                    if game.memory_mb >= 1024 && game.memory_mb % 1024 == 0 { format!("{} GB", game.memory_mb / 1024) } else { format!("{} MB", game.memory_mb) }
                ),
                i,
            );
        }
        // com/example/Foo has been compiled by a more recent version of the Java Runtime (class file version 65.0),
        // this version of the Java Runtime only recognizes class file versions up to 61.0
        if l.contains("UnsupportedClassVersionError") || l.contains("has been compiled by a more recent version of the Java Runtime") {
            let java = |v: Option<&str>| v.and_then(|v| v.split('.').next()?.parse::<u32>().ok()).map(|v| v.saturating_sub(44));
            let (needs, has) = (java(between(l, "class file version ", ")")), java(l.split("class file versions up to ").nth(1)));
            let class = l.split_whitespace().find(|w| w.contains('/') && !w.starts_with("java/")).map(|c| c.replace('/', "."));
            let needs_txt = needs.map(|n| format!("Java {n}")).unwrap_or("a newer Java".into());
            let has_txt = has.map(|n| format!("Java {n}")).unwrap_or("an older one".into());
            match class.as_deref().and_then(|c| self.ix.by_class(c)) {
                Some(m) => {
                    let id = m.facts.id.clone().unwrap_or_else(|| file_stem(&m.file));
                    self.hit(&id, None, None, Confidence::Certain, format!("needs {needs_txt}, but this Minecraft runs on {has_txt}: it's made for a newer Minecraft"), i)
                }
                None if !l.trim_start().starts_with("at ") => self.cause(
                    "wrong java version",
                    format!("Something in this game needs {needs_txt}, but it runs on {has_txt}. Usually a mod made for a newer Minecraft; the next lines may name it."),
                    i,
                ),
                None => {}
            }
        }
        const GL: &[&str] = &[
            "No OpenGL context found",
            "GLFW error 65542",
            "GLFW error 65543",
            "GLFW error 65545",
            "GLX: No GLXFBConfigs returned",
            "Failed to create the GLFW window",
            "WGL: The driver does not appear to support OpenGL",
            "OpenGL 3.2 is not supported",
            "Couldn't set pixel format",
            "Pixel format not accelerated",
            "EGL: Failed to",
        ];
        if GL.iter().any(|g| l.contains(g)) {
            self.cause(
                "graphics driver problem",
                "Minecraft couldn't start OpenGL. Update your graphics drivers (Mesa, or NVIDIA's), and make sure the game runs on a GPU that has them.".into(),
                i,
            );
        }
        if l.contains("A fatal error has been detected by the Java Runtime Environment") {
            self.cause(
                "java itself crashed",
                "The Java runtime crashed outside Minecraft's own code, most often in a graphics driver. The hs_err_pid file in the instance folder has the details.".into(),
                i,
            );
        }
    }

    /// Mods in stack traces: Forge's module frames (`at TRANSFORMER/sodium@0.5.8/…`),
    /// mixin handlers (`handler$zza000$sodium$onRender`) and classes in a mod's packages.
    /// A mod near the top of a trace is likely; further down, possible.
    fn stack_frames(&mut self) {
        let mut depth = 0;
        for i in 0..self.lines.len() {
            let t = self.lines[i].trim();
            let Some(frame) = t.strip_prefix("at ") else {
                depth = 0;
                continue;
            };
            depth += 1;
            let conf = if depth <= 8 { Confidence::Likely } else { Confidence::Possible };
            let place = if depth == 1 { "at the top of the stack trace" } else { "in the crash's stack trace" };
            // (id, version, what of it is there)
            let mut found: Vec<(String, Option<String>, &str)> = Vec::new();
            let method = frame.split('(').next().unwrap_or(frame);
            // Forge/NeoForge: "at TRANSFORMER/sodium@0.5.8/me.jellysquid…" or "at sodium@0.5.8/…"
            let module = method.split('/').find(|p| p.contains('@')).unwrap_or("");
            if let Some((id, version)) = module.split_once('@')
                && is_id(id)
                && !id.starts_with("java.")
                && !id.starts_with("jdk.")
            {
                found.push((id.to_string(), Some(version.to_string()), "its code is"));
            }
            let class_path = method.rsplit('/').next().unwrap_or(method);
            // Mixin handlers carry the mod id: handler$zza000$sodium$onRender.
            for prefix in ["handler$", "redirect$", "modify$", "localvar$", "wrapOperation$", "wrapWithCondition$", "modifyExpressionValue$", "modifyReturnValue$"] {
                if let Some(rest) = class_path.split(prefix).nth(1) {
                    let mut parts = rest.split('$');
                    if let (Some(_), Some(id)) = (parts.next(), parts.next())
                        && parts.next().is_some()
                        && is_id(id)
                    {
                        found.push((id.to_string(), None, "its mixin is"));
                    }
                }
            }
            if found.is_empty()
                && let Some((class, _)) = class_path.rsplit_once('.')
                && let Some(m) = self.ix.by_class(class)
            {
                found.push((m.facts.id.clone().unwrap_or_else(|| file_stem(&m.file)), None, "its code is"));
            }
            for (id, version, what) in found {
                self.hit(&id, None, version.as_deref(), conf, format!("{what} {place}"), i);
            }
        }
    }

    /// Jars written for another loader than the instance's, among the enabled ones.
    fn wrong_loader(&mut self, game: Game) {
        use JarKind::*;
        let connector = self.ix.by_id("connector").is_some_and(|m| m.enabled);
        let mut found = Vec::new();
        for m in self.ix.mods.iter().filter(|m| m.enabled) {
            let k = &m.facts.kinds;
            if k.is_empty() {
                continue;
            }
            let fits = match game.loader {
                LoaderKind::Fabric => k.contains(&Fabric),
                LoaderKind::Quilt => k.contains(&Fabric) || k.contains(&Quilt),
                LoaderKind::Forge => k.contains(&Forge) || k.contains(&LegacyForge) || (connector && k.contains(&Fabric)),
                LoaderKind::NeoForge => k.contains(&NeoForge) || k.contains(&Forge) || (connector && k.contains(&Fabric)),
                LoaderKind::Vanilla => true,
            };
            if !fits {
                let made = match k[0] {
                    Fabric => "Fabric",
                    Quilt => "Quilt",
                    Forge | LegacyForge => "Forge",
                    NeoForge => "NeoForge",
                    Unknown => continue,
                };
                let runs = match game.loader {
                    LoaderKind::Fabric => "Fabric",
                    LoaderKind::Quilt => "Quilt",
                    LoaderKind::Forge => "Forge",
                    LoaderKind::NeoForge => "NeoForge",
                    LoaderKind::Vanilla => "no loader",
                };
                found.push((m.facts.id.clone().unwrap_or_else(|| file_stem(&m.file)), format!("is a {made} mod; this instance runs {runs}")));
            }
        }
        for (id, reason) in found {
            // No log line says this: it comes from the jar, so it carries no evidence.
            self.hits.push(Hit { id: id.to_lowercase(), name: None, version: None, confidence: Confidence::Likely, reason, line: usize::MAX });
        }
    }

    /// One suspect per mod: the strongest reason, every line that points at it.
    fn merge(&mut self) -> Vec<Suspect> {
        let mut by: BTreeMap<String, (Suspect, usize)> = BTreeMap::new();
        for h in self.hits.drain(..) {
            // Nested ids (a Fabric API module) are blamed on the jar that carries them.
            let entry = self.ix.by_id(&h.id);
            let key = entry.and_then(|m| m.facts.id.clone()).unwrap_or_else(|| h.id.clone()).to_lowercase();
            let s = by.entry(key.clone()).or_insert_with(|| {
                let (name, version, file, sha1) = match entry {
                    Some(m) => (m.name(), m.facts.version.clone().or(h.version.clone()), Some(m.file.clone()), m.facts.sha1.clone()),
                    None => (h.name.clone().unwrap_or_else(|| h.id.clone()), h.version.clone(), None, None),
                };
                (Suspect { id: key, name, version, file, sha1, confidence: h.confidence, reason: h.reason.clone(), evidence: Vec::new() }, 0)
            });
            s.1 += 1;
            if h.confidence > s.0.confidence {
                s.0.confidence = h.confidence;
                s.0.reason = h.reason.clone();
            }
            if h.line != usize::MAX && s.0.evidence.len() < EVIDENCE_LINES {
                let e = evidence(&self.lines, h.line);
                if !s.0.evidence.iter().any(|x| x.text == e.text) {
                    s.0.evidence.push(e);
                }
            }
        }
        let mut out: Vec<(Suspect, usize)> = by.into_values().collect();
        // Strongest first, then the most often named, then the earliest in the log.
        out.sort_by(|a, b| {
            b.0.confidence.cmp(&a.0.confidence).then(b.1.cmp(&a.1)).then(a.0.evidence.first().map(|e| e.line).cmp(&b.0.evidence.first().map(|e| e.line)))
        });
        out.into_iter().map(|(s, _)| s).collect()
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// "java.lang.NullPointerException: Cannot invoke …" → "NullPointerException".
fn short_exception(line: &str) -> String {
    let head = line.trim().split(':').next().unwrap_or("").trim();
    if head.contains(' ') {
        return String::new();
    }
    head.rsplit('.').next().unwrap_or(head).to_string()
}

// -- finding the crash --------------------------------------------------------------

/// The newest crash report written at or after `since`.
pub fn crash_report_since(inst: &Instance, since: SystemTime) -> Option<PathBuf> {
    std::fs::read_dir(inst.crash_reports_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".txt"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(m, _)| *m >= since)
        .max()
        .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entry(id: &str, name: &str, version: &str, packages: &[&str], mixins: &[&str], kinds: &[JarKind], enabled: bool) -> ModEntry {
        ModEntry {
            file: PathBuf::from(format!("/mods/{id}-{version}.jar")),
            enabled,
            facts: Arc::new(JarFacts {
                id: Some(id.into()),
                name: Some(name.into()),
                version: Some(version.into()),
                sha1: None,
                ids: vec![id.into()],
                kinds: kinds.to_vec(),
                packages: packages.iter().map(|p| p.to_string()).collect(),
                mixin_configs: mixins.iter().map(|p| p.to_string()).collect(),
            }),
        }
    }

    /// Loader unknown, so the wrong-loader check stays out of the way.
    fn fabric_pack() -> ModIndex {
        ModIndex::new(vec![
            entry("sodium", "Sodium", "0.5.8", &["me.jellysquid.mods.sodium.client.render.chunk"], &["sodium.mixins.json"], &[], true),
            entry("iris", "Iris", "1.7.0", &["net.irisshaders.iris.pipeline"], &["mixins.iris.json"], &[], true),
            entry("lithium", "Lithium", "0.12.1", &["me.jellysquid.mods.lithium.common"], &[], &[], true),
            entry("examplemod", "Example Mod", "1.0", &["com.example.examplemod", "com.example.examplemod.client"], &[], &[], true),
        ])
    }

    const FABRIC: Game = Game { loader: LoaderKind::Fabric, memory_mb: 4096 };
    const FORGE: Game = Game { loader: LoaderKind::Forge, memory_mb: 4096 };

    fn top(a: &Analysis) -> (&str, Confidence) {
        let s = a.suspects.first().expect("a suspect");
        (s.id.as_str(), s.confidence)
    }

    #[test]
    fn forge_mod_loading_error_names_the_mod() {
        let report = "\
---- Minecraft Crash Report ----
// Who set us up the TNT?

Time: 2026-10-17 21:04:11
Description: Mod loading error has occurred

java.lang.Exception: Mod Loading has failed
\tat net.minecraftforge.logging.CrashReportExtender.dumpModLoadingCrashReport(CrashReportExtender.java:55) ~[forge-1.20.1-47.2.0-universal.jar%23192!/:?] {re:classloading}

A detailed walkthrough of the error, its code path and all known details is as follows:
---------------------------------------------------------------------------------------

-- Head --
Thread: Render thread
Suspected Mod: NONE
Stacktrace:
\tat net.minecraftforge.logging.CrashReportExtender.lambda$dumpModLoadingCrashReport$7(CrashReportExtender.java:58) ~[forge-1.20.1-47.2.0-universal.jar%23192!/:?] {re:classloading}

-- MOD examplemod --
Details:
\tCaught exception: java.lang.NullPointerException: Cannot invoke \"net.minecraft.world.item.Item.getDefaultInstance()\" because \"item\" is null
\tMod File: /home/u/.local/share/omacraft/instances/x/minecraft/mods/examplemod-1.0.jar
\tFailure message: Example Mod (examplemod) has failed to load correctly
\t\tjava.lang.NullPointerException: Cannot invoke \"net.minecraft.world.item.Item.getDefaultInstance()\" because \"item\" is null
\tMod Version: 1.0
\tMod Issue URL: NOT PROVIDED
\tException message: java.lang.NullPointerException
";
        let a = analyze_with(report, &fabric_pack(), FORGE);
        assert!(a.crashed);
        assert_eq!(a.description.as_deref(), Some("Mod loading error has occurred"));
        assert_eq!(top(&a), ("examplemod", Confidence::Certain));
        assert_eq!(a.suspects[0].name, "Example Mod");
        assert!(a.suspects[0].reason.contains("NullPointerException"), "{}", a.suspects[0].reason);
        assert_eq!(a.headline(), "cause: Example Mod 1.0");
        assert_eq!(a.suspects.len(), 1, "{:?}", a.suspects.iter().map(|s| &s.id).collect::<Vec<_>>());
    }

    #[test]
    fn neoforge_missing_dependency_names_the_mod_that_needs_it() {
        let log = "\
[17Oct2026 21:10:02.114] [main/ERROR] [net.neoforged.fml.loading.ModSorter/LOADING]: Missing or unsupported mandatory dependencies:
Mod Create Crafts & Additions (createaddition) requires create 0.5.1 or above
Currently, create is not installed
[17Oct2026 21:10:03.000] [main/FATAL] [net.neoforged.fml.ModLoader/LOADING]: Loading errors encountered
";
        let a = analyze_with(log, &ModIndex::default(), Game { loader: LoaderKind::NeoForge, memory_mb: 4096 });
        assert!(a.crashed);
        assert_eq!(top(&a), ("createaddition", Confidence::Certain));
        assert_eq!(a.suspects[0].name, "Create Crafts & Additions");
        assert_eq!(a.suspects[0].reason, "needs create 0.5.1 or above, which isn't installed");
        assert_eq!(a.suspects[0].evidence[0].line, 1);

        // Forge 1.20's wording.
        let log = "Missing or unsupported mandatory dependencies:\n\tMod ID: 'create', Requested by: 'createaddition', Expected range: '[0.5.1,)', Actual version: '[MISSING]'\n";
        let a = analyze_with(log, &ModIndex::default(), FORGE);
        assert_eq!(top(&a), ("createaddition", Confidence::Certain));
        assert_eq!(a.suspects[0].reason, "needs create [0.5.1,), which isn't installed");
    }

    #[test]
    fn fabric_incompatible_mods_explain_what_each_needs() {
        let log = "\
[21:14:02] [main/ERROR]: Incompatible mods found!
net.fabricmc.loader.impl.FormattedException: Some of your mods are incompatible with the game or each other!
A potential solution has been determined, this may resolve your problem:
\t - Replace mod 'Sodium' (sodium) 0.4.0 with version 0.5.8 or later.
\t - Install fabric-api, any version.
More details:
\t - Mod 'Iris' (iris) 1.7.0 requires version 0.5.8 or later of mod 'Sodium' (sodium), but only the wrong version is present: 0.4.0!
\t - Mod 'Mod Menu' (modmenu) 11.0.1 requires any version of fabric-api, which is missing!
\t - Mod 'Old Thing' (oldthing) 2.0 requires version 1.20.4 of minecraft, but only the wrong version is present: 1.21.1!
";
        let a = analyze_with(log, &fabric_pack(), FABRIC);
        assert!(a.crashed);
        let by = |id: &str| a.suspects.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("no {id}"));
        assert_eq!(by("iris").confidence, Confidence::Certain);
        assert_eq!(by("iris").reason, "needs Sodium 0.5.8 or later, but 0.4.0 is installed");
        assert_eq!(by("modmenu").reason, "needs fabric-api, which isn't installed");
        assert_eq!(by("modmenu").name, "Mod Menu");
        assert_eq!(by("oldthing").reason, "is made for Minecraft 1.20.4; this is 1.21.1");
        // The "potential solution" lines don't add suspects of their own.
        assert_eq!(a.suspects.len(), 3);
        assert_eq!(a.headline(), "causes: Iris 1.7.0, Mod Menu 11.0.1 and 1 more");
    }

    #[test]
    fn a_failed_mixin_is_certain() {
        let log = "\
[21:20:11] [Render thread/ERROR]: Mixin apply for mod sodium failed sodium.mixins.json:core.MixinLevelRenderer from mod sodium -> net.minecraft.client.renderer.LevelRenderer: org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException Critical injection failure
org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException: Critical injection failure: @Inject annotation on onRender could not find any targets matching 'renderLevel' in net/minecraft/client/renderer/LevelRenderer. [PREINJECT Applicator Phase -> sodium.mixins.json:core.MixinLevelRenderer from mod sodium -> Prepare Injections -> handler$zza000$sodium$onRender]
\tat org.spongepowered.asm.mixin.injection.struct.InjectionInfo.validateTargets(InjectionInfo.java:656) ~[sponge-mixin-0.15.jar:?]
";
        let a = analyze_with(log, &fabric_pack(), FABRIC);
        assert_eq!(top(&a), ("sodium", Confidence::Certain));
        assert_eq!(a.suspects[0].reason, "its mixin into LevelRenderer failed");
        assert_eq!(a.suspects[0].label(), "Sodium 0.5.8");
        assert_eq!(a.suspects.len(), 1);
        assert_eq!(a.headline(), "cause: Sodium 0.5.8");
    }

    #[test]
    fn a_failed_entrypoint_is_certain() {
        let log = "\
[21:30:00] [main/ERROR]: Could not execute entrypoint stage 'client' due to errors, provided by 'examplemod' at 'com.example.examplemod.client.ExampleClient'!
java.lang.IllegalStateException: Registry is already frozen
\tat net.minecraft.class_2370.method_10272(class_2370.java:201) ~[client-intermediary.jar:?]
\tat com.example.examplemod.client.ExampleClient.onInitializeClient(ExampleClient.java:22) ~[examplemod-1.0.jar:?]
";
        let a = analyze_with(log, &fabric_pack(), FABRIC);
        assert!(a.crashed);
        assert_eq!(top(&a), ("examplemod", Confidence::Certain));
        assert_eq!(a.suspects[0].reason, "crashed while starting up (its client entrypoint)");
        assert_eq!(a.suspects[0].evidence.len(), 2, "the frame adds evidence");
    }

    #[test]
    fn a_stack_trace_alone_is_matched_to_jars_by_package() {
        let report = "\
---- Minecraft Crash Report ----
Description: Ticking entity

java.lang.ArrayIndexOutOfBoundsException: Index 16 out of bounds for length 16
\tat me.jellysquid.mods.lithium.common.world.chunk.LithiumHashPalette.get(LithiumHashPalette.java:120) ~[lithium-0.12.1.jar:?]
\tat net.minecraft.class_2841.method_12321(class_2841.java:88) ~[client-intermediary.jar:?]
\tat net.minecraft.class_1937.method_8320(class_1937.java:300) ~[client-intermediary.jar:?]
\tat com.google.common.collect.Iterators.forEach(Iterators.java:10) ~[guava.jar:?]
\tat java.base/java.lang.Thread.run(Thread.java:1583) [?:?]
";
        let a = analyze_with(report, &fabric_pack(), FABRIC);
        assert!(a.crashed);
        assert_eq!(top(&a), ("lithium", Confidence::Likely));
        assert_eq!(a.suspects[0].reason, "its code is at the top of the stack trace");
        assert_eq!(a.headline(), "likely cause: Lithium 0.12.1");
        assert_eq!(a.suspects.len(), 1, "minecraft, guava and java frames point at nothing");
        assert_eq!(a.description.as_deref(), Some("Ticking entity"));
    }

    #[test]
    fn mixin_handlers_and_forge_module_frames_name_their_mods() {
        let report = "\
---- Minecraft Crash Report ----
Description: Rendering overlay

java.lang.NullPointerException
\tat net.minecraft.class_761.handler$zza000$iris$onRender(class_761.java:2000) ~[client-intermediary.jar:?]
\tat TRANSFORMER/sodium@0.5.8/me.jellysquid.mods.sodium.client.render.chunk.Foo.bar(Foo.java:10) ~[sodium-0.5.8.jar%23100!/:?]
\tat TRANSFORMER/minecraft@1.20.1/net.minecraft.client.Minecraft.run(Minecraft.java:700) ~[client.jar:?]
";
        let a = analyze_with(report, &fabric_pack(), FORGE);
        let ids: Vec<&str> = a.suspects.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["iris", "sodium"]);
        assert!(a.suspects.iter().all(|s| s.confidence == Confidence::Likely));
    }

    #[test]
    fn suspected_mods_are_likely() {
        let report = "\
---- Minecraft Crash Report ----
Description: Unexpected error

-- Head --
Thread: Render thread
Suspected Mods:
\tIris (iris), Version: 1.7.0
\t\tIssue tracker URL: https://github.com/IrisShaders/Iris/issues
\t\tat TRANSFORMER/iris@1.7.0/net.irisshaders.iris.pipeline.Pipeline.begin(Pipeline.java:40)
Stacktrace:
\tat net.minecraft.client.Minecraft.run(Minecraft.java:700) ~[client.jar:?]
";
        let a = analyze_with(report, &fabric_pack(), FORGE);
        assert_eq!(top(&a), ("iris", Confidence::Likely));
        assert_eq!(a.suspects[0].reason, "Minecraft's crash report suspects it");
        assert_eq!(a.headline(), "likely cause: Iris 1.7.0");
    }

    #[test]
    fn running_out_of_memory_is_explained() {
        let log = "\
[22:01:00] [Server thread/ERROR]: Encountered an unexpected exception
java.lang.OutOfMemoryError: Java heap space
\tat java.base/java.util.Arrays.copyOf(Arrays.java:3512) ~[?:?]
";
        let a = analyze_with(log, &fabric_pack(), Game { loader: LoaderKind::Fabric, memory_mb: 2048 });
        assert!(a.crashed);
        assert!(a.suspects.is_empty());
        assert_eq!(a.headline(), "out of memory");
        assert!(a.causes[0].advice.contains("2 GB"), "{}", a.causes[0].advice);
    }

    #[test]
    fn a_class_for_a_newer_java_names_its_mod() {
        let log = "\
java.lang.UnsupportedClassVersionError: com/example/examplemod/ExampleMod has been compiled by a more recent version of the Java Runtime (class file version 65.0), this version of the Java Runtime only recognizes class file versions up to 61.0
";
        let a = analyze_with(log, &fabric_pack(), FABRIC);
        assert_eq!(top(&a), ("examplemod", Confidence::Certain));
        assert_eq!(a.suspects[0].reason, "needs Java 21, but this Minecraft runs on Java 17: it's made for a newer Minecraft");
        // A class no jar holds is still explained.
        let log = log.replace("com/example/examplemod", "org/unknown");
        let a = analyze_with(&log, &fabric_pack(), FABRIC);
        assert!(a.suspects.is_empty());
        assert_eq!(a.causes[0].title, "wrong java version");
    }

    #[test]
    fn graphics_driver_errors_are_explained() {
        let log = "[22:05:00] [Render thread/ERROR]: GLFW error 65542: GLX: No GLXFBConfigs returned\n#@!@# Game crashed! Crash report saved to: #@!@# /x/crash-reports/crash.txt\n";
        let a = analyze_with(log, &fabric_pack(), FABRIC);
        assert_eq!(a.headline(), "graphics driver problem");
        assert_eq!(a.causes[0].evidence.len(), 1);
    }

    #[test]
    fn a_mod_for_another_loader_is_pointed_out() {
        let ix = ModIndex::new(vec![
            entry("sodium", "Sodium", "0.5.8", &[], &[], &[JarKind::Fabric], true),
            entry("create", "Create", "0.5.1", &[], &[], &[JarKind::Forge], true),
            entry("jei", "JEI", "15", &[], &[], &[JarKind::Forge], false),
        ]);
        let a = analyze_with("---- Minecraft Crash Report ----\nDescription: Initializing game\n", &ix, FABRIC);
        assert_eq!(top(&a), ("create", Confidence::Likely));
        assert_eq!(a.suspects[0].reason, "is a Forge mod; this instance runs Fabric");
        assert_eq!(a.suspects.len(), 1, "a disabled jar isn't loaded");
        // A log without a crash doesn't go looking.
        assert!(analyze_with("[main/INFO]: Loading 3 mods\n", &ix, FABRIC).suspects.is_empty());
    }

    #[test]
    fn no_clear_culprit_says_so() {
        let report = "\
---- Minecraft Crash Report ----
Description: Unexpected error

java.lang.IllegalStateException: Not on the render thread
\tat net.minecraft.client.renderer.RenderSystem.assertOnRenderThread(RenderSystem.java:40) ~[client.jar:?]
\tat net.minecraft.client.Minecraft.run(Minecraft.java:700) ~[client.jar:?]
\tat java.base/java.lang.Thread.run(Thread.java:1583) [?:?]

-- System Details --
Fabric Mods:
\tsodium: Sodium 0.5.8
\tlithium: Lithium 0.12.1
";
        let a = analyze_with(report, &fabric_pack(), FABRIC);
        assert!(a.crashed);
        assert!(a.suspects.is_empty(), "{:?}", a.suspects.iter().map(|s| &s.id).collect::<Vec<_>>());
        assert!(a.culprit().is_none());
        assert_eq!(a.headline(), "no single mod stands out");
        assert!(a.summary().contains("No mod or other cause stands out"));
        // Mods named only deep in a trace stay "possible", and don't make a headline.
        let deep: String = (0..10).map(|i| format!("\tat net.minecraft.A.m{i}(A.java:1)\n")).collect();
        let a = analyze_with(&format!("---- Minecraft Crash Report ----\njava.lang.RuntimeException\n{deep}\tat me.jellysquid.mods.lithium.common.X.y(X.java:1)\n"), &fabric_pack(), FABRIC);
        assert_eq!(top(&a), ("lithium", Confidence::Possible));
        assert_eq!(a.headline(), "no single mod stands out");
    }

    #[test]
    fn a_log_without_a_crash_is_not_one() {
        let a = analyze_with("[21:00:00] [main/INFO]: Loading Minecraft 1.21.1 with Fabric Loader 0.16.5\n[21:00:09] [Render thread/INFO]: Stopping!\n", &fabric_pack(), FABRIC);
        assert!(!a.crashed);
        assert_eq!(a.headline(), "no crash found");
    }

    #[test]
    fn only_crash_reports_from_this_run_count() {
        let mut inst = Instance::new("crash since", "1.21.1", crate::loaders::Loader::Vanilla);
        inst.assign_id().unwrap();
        let dir = inst.crash_reports_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("crash-old-client.txt");
        std::fs::write(&old, "---- Minecraft Crash Report ----\n").unwrap();
        let started = SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(crash_report_since(&inst, started), None, "an older report isn't this run's");
        let new = dir.join("crash-new-client.txt");
        std::fs::write(&new, "---- Minecraft Crash Report ----\n").unwrap();
        let f = std::fs::File::options().write(true).open(&new).unwrap();
        f.set_modified(started + std::time::Duration::from_secs(5)).unwrap();
        std::fs::write(dir.join("notes.log"), "x").unwrap();
        assert_eq!(crash_report_since(&inst, started), Some(new));
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn jar_facts_index_packages_mixins_and_nested_jars() {
        fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            for (name, bytes) in entries {
                z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(bytes).unwrap();
            }
            z.finish().unwrap().into_inner()
        }
        let inner = jar(&[("fabric.mod.json", br#"{"id": "bundled-lib", "version": "2.0"}"#), ("org/bundled/lib/Thing.class", b"x")]);
        let outer = jar(&[
            ("fabric.mod.json", br#"{"schemaVersion": 1, "id": "lithium", "name": "Lithium", "version": "0.12.1", "jars": [{"file": "META-INF/jars/lib.jar"}]}"#),
            ("me/jellysquid/mods/lithium/common/X.class", b"x"),
            ("lithium.mixins.json", b"{}"),
            ("META-INF/jars/lib.jar", &inner),
        ]);
        let path = crate::paths::test_root().join(format!("crash-{}.jar", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, outer).unwrap();
        let f = facts(&path);
        assert_eq!(f.id.as_deref(), Some("lithium"));
        assert_eq!(f.kinds, [JarKind::Fabric]);
        assert_eq!(f.packages, ["me.jellysquid.mods.lithium.common", "org.bundled.lib"]);
        assert_eq!(f.mixin_configs, ["lithium.mixins.json"]);
        assert!(f.ids.contains(&"bundled-lib".to_string()));
        assert!(Arc::ptr_eq(&f, &facts(&path)), "unchanged jars come from the cache");

        let ix = ModIndex::new(vec![ModEntry { file: path.clone(), enabled: true, facts: f }]);
        let a = analyze_with("java.lang.RuntimeException: boom\n\tat org.bundled.lib.Thing.go(Thing.java:1)\n", &ix, FABRIC);
        assert_eq!(top(&a), ("lithium", Confidence::Likely), "a bundled library's frame is its mod's");
        assert_eq!(a.suspects[0].file.as_deref(), Some(path.as_path()));
        std::fs::remove_file(path).unwrap();
    }
}
