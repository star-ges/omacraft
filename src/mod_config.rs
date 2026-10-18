//! Mod config files: an instance's `config/` folder listed and grouped by the mod each
//! file belongs to, TOML/JSON/JSON5 read into editable fields, and edits written back.
//!
//! Edits never re-serialize a file: each field remembers where its value sits in the
//! text, and a change replaces just those bytes. So every comment, blank line, key
//! order and quirk of the original survives a save. TOML is read with `toml_edit`
//! (which keeps those positions); JSON and JSON5 with the small parser below, which
//! also keeps the comments mods put there to explain their options.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail};

use crate::fsutil;
use crate::instance::Instance;

/// The biggest config file the editor opens. Configs are small; anything bigger is
/// data, not settings.
pub const MAX_OPEN: u64 = 1 << 20;
/// How deep and how many files the listing goes into `config/`.
const MAX_DEPTH: usize = 8;
const MAX_FILES: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
    Json5,
    /// Edited as text only: `.properties`, `.cfg`, `.ini`, `.txt` and the like.
    Text,
    /// Minecraft's own `options.txt`: `key:value` lines (see `vanilla_options`).
    Options,
    /// A shader pack's settings file (`shaderpacks/<pack>.txt`): `NAME=value` lines for the options changed from
    /// the pack's defaults, which come from the pack itself (see `shader_config`).
    ShaderSettings,
}

impl Format {
    /// What a file is, by its extension; `None` for files the editor leaves alone.
    pub fn of(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        Some(match ext.as_str() {
            "toml" => Format::Toml,
            "json" | "mcmeta" => Format::Json,
            "json5" | "jsonc" | "hjson" => Format::Json5,
            "properties" | "cfg" | "conf" | "txt" | "ini" | "yml" | "yaml" | "snbt" => Format::Text,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Toml => "toml",
            Format::Json => "json",
            Format::Json5 => "json5",
            Format::Text => "text",
            Format::Options => "minecraft options",
            Format::ShaderSettings => "shader settings",
        }
    }

    pub fn structured(self) -> bool {
        self != Format::Text
    }
}

// -- listing --------------------------------------------------------------------

pub fn config_dir(inst: &Instance) -> PathBuf {
    inst.game_dir().join("config")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    /// Relative to `config/`.
    pub rel: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Every editable file under `config/`, sorted by path. Links are followed only while
/// they stay inside `config/`.
pub fn list(inst: &Instance) -> Vec<ConfigFile> {
    let root = config_dir(inst);
    let Ok(canon) = root.canonicalize() else { return Vec::new() };
    let mut out = Vec::new();
    let mut stack = vec![(root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if out.len() >= MAX_FILES {
                break;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            // Whatever it is (a link included), it must really live in config/.
            let Ok(real) = path.canonicalize() else { continue };
            if !real.starts_with(&canon) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_dir() {
                if depth + 1 < MAX_DEPTH && !e.file_type().is_ok_and(|t| t.is_symlink()) {
                    stack.push((path, depth + 1));
                }
            } else if meta.is_file() && Format::of(&path).is_some() {
                let rel = path.strip_prefix(&root).map(Path::to_path_buf).unwrap_or_default();
                out.push(ConfigFile { rel, size: meta.len(), modified: meta.modified().ok() });
            }
        }
    }
    out.sort_by_key(|f| f.rel.to_string_lossy().to_lowercase());
    out
}

/// A mod, for naming the files that belong to it.
#[derive(Debug, Clone, Default)]
pub struct ModInfo {
    pub name: String,
    /// Its own id first, then any others its jar declares.
    pub ids: Vec<String>,
    pub icon: Option<PathBuf>,
}

/// Read what each of the instance's mods calls itself. `cache` keeps jars already read
/// (by path, size and mtime), so a refresh only opens new or changed ones.
pub fn known_mods(inst: &Instance, cache: &mut HashMap<String, ModInfo>) -> Vec<ModInfo> {
    let mut seen = HashMap::new();
    for m in inst.mods() {
        let stamp = m.modified.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        let key = format!("{}|{}|{stamp}", m.path.display(), m.size);
        let info = cache.entry(key.clone()).or_insert_with(|| {
            let meta = crate::content_meta::read_quick(&m.path);
            let mut ids: Vec<String> = meta.mod_id.into_iter().collect();
            ids.extend(crate::world_sets::mod_inventory(&m.path).into_keys().filter(|id| !ids.contains(id)).collect::<Vec<_>>());
            ModInfo { name: meta.name.unwrap_or_else(|| m.display_name().to_string()), ids, icon: meta.icon }
        });
        seen.insert(key, info.clone());
    }
    *cache = seen.clone();
    let mut out: Vec<ModInfo> = seen.into_values().collect();
    out.sort_by_key(|m| m.name.to_lowercase());
    out
}

/// Files under the mod they belong to.
#[derive(Debug, Clone)]
pub struct Group {
    /// The mod's name, or "other".
    pub name: String,
    pub icon: Option<PathBuf>,
    /// The mod ids its jar declares (none for "other"), so a mod in the mods tab can find its configs.
    pub ids: Vec<String>,
    pub files: Vec<ConfigFile>,
}

pub const OTHER: &str = "other";

fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// The word a config file is named by: its top folder, or its name without the
/// extension and the usual `-client`/`-common`/`-server` endings.
fn file_word(rel: &Path) -> String {
    let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
    let mut word = if rel.components().count() > 1 { first } else { first.split('.').next().unwrap_or_default().to_string() };
    loop {
        let lower = word.to_ascii_lowercase();
        let cut = ["-client", "_client", "-common", "_common", "-server", "_server", "-options", "_options", "-config", "_config", "-settings", "-mod"]
            .iter()
            .find(|s| lower.ends_with(*s) && lower.len() > s.len());
        match cut {
            Some(s) => word.truncate(word.len() - s.len()),
            None => break,
        }
    }
    squash(&word)
}

/// How well a file's word names a mod: exact id or name beats a prefix either way.
fn score(word: &str, m: &ModInfo) -> u32 {
    if word.is_empty() {
        return 0;
    }
    let names = m.ids.iter().map(|i| squash(i)).chain([squash(&m.name)]).filter(|n| n.len() >= 2);
    names
        .map(|n| {
            if n == word {
                1000 + n.len() as u32
            } else if n.len() >= 4 && word.starts_with(&n) {
                500 + n.len() as u32
            } else if word.len() >= 4 && n.starts_with(word) {
                400 + word.len() as u32
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0)
}

/// Files grouped by mod, mods by name, "other" last.
pub fn group(files: &[ConfigFile], mods: &[ModInfo]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for f in files {
        let word = file_word(&f.rel);
        let best = mods.iter().map(|m| (score(&word, m), m)).filter(|(s, _)| *s > 0).max_by_key(|(s, _)| *s).map(|(_, m)| m);
        let (name, icon, ids) = match best {
            Some(m) => (m.name.clone(), m.icon.clone(), m.ids.clone()),
            None => (OTHER.to_string(), None, Vec::new()),
        };
        match groups.iter_mut().find(|g| g.name == name) {
            Some(g) => g.files.push(f.clone()),
            None => groups.push(Group { name, icon, ids, files: vec![f.clone()] }),
        }
    }
    groups.sort_by_key(|g| (g.name == OTHER, g.name.to_lowercase()));
    groups
}

// -- reading and saving ---------------------------------------------------------

/// Size and mtime, to notice a file changing under the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Stamp {
    pub fn of(path: &Path) -> Option<Stamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(Stamp { len: m.len(), modified: m.modified().ok() })
    }
}

/// Where `rel` really is, refusing anything that leads out of `config/` (`..`, or a
/// link pointing elsewhere).
pub fn resolve(inst: &Instance, rel: &Path) -> anyhow::Result<PathBuf> {
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        bail!("{} isn't a path inside config/", rel.display());
    }
    let root = config_dir(inst).canonicalize().context("the instance has no config folder")?;
    let real = config_dir(inst).join(rel).canonicalize().with_context(|| format!("{} is gone", rel.display()))?;
    if !real.starts_with(&root) {
        bail!("{} leads outside the config folder", rel.display());
    }
    Ok(real)
}

/// A file as read for editing.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub text: String,
    pub stamp: Option<Stamp>,
    pub format: Format,
    /// For shader settings: the pack's options, which the file's values are laid over.
    pub base: Option<std::sync::Arc<Doc>>,
}

/// A config file's text, refusing big or non-text files.
pub fn read_text(path: &Path) -> anyhow::Result<String> {
    let f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > MAX_OPEN {
        bail!("{} is {} kB, too big to edit here; open it from the folder instead", path.file_name().unwrap_or_default().to_string_lossy(), len / 1000);
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    f.take(MAX_OPEN + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_OPEN {
        bail!("it grew past {} kB while being read", MAX_OPEN / 1000);
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("it isn't text (not UTF-8), so it can't be edited here"))
}

pub fn load(inst: &Instance, rel: &Path) -> anyhow::Result<Loaded> {
    let path = resolve(inst, rel)?;
    let text = read_text(&path)?;
    Ok(Loaded { stamp: Stamp::of(&path), format: Format::of(rel).unwrap_or(Format::Text), base: None, path, text })
}

#[derive(Debug)]
pub enum Saved {
    Done(Option<Stamp>),
    /// The file on disk isn't what was loaded any more (the game rewrote it): its text now.
    Changed(String),
}

/// Write `new` over `path`, but only if the file still holds `expected` (what the
/// editor loaded), unless `force`. Keeps the file's permissions.
pub fn save(path: &Path, expected: &str, new: &str, force: bool) -> anyhow::Result<Saved> {
    let now = match read_text(path) {
        Ok(t) => Some(t),
        Err(_) if !path.exists() => None,
        Err(e) => return Err(e),
    };
    // A file that isn't there yet (a shader pack's settings before any change) counts as empty.
    if !force && now.as_deref().unwrap_or_default() != expected {
        return Ok(Saved::Changed(now.unwrap_or_default()));
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o644);
    fsutil::write_atomic(path, new.as_bytes(), mode)?;
    Ok(Saved::Done(Stamp::of(path)))
}

// -- fields ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Scalar {
    pub fn display(&self) -> String {
        match self {
            Scalar::Bool(b) => b.to_string(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => float_text(*f),
            Scalar::Str(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Scalar(Scalar),
    /// A list of plain values. `editable` is false if it holds anything else, or
    /// comments that rewriting it would lose.
    List { items: Vec<Scalar>, editable: bool },
    /// A table inside a list, a date, `null`…: shown, edited only as text.
    Other(String),
}

/// What a field's comment says about its values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hint {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub allowed: Vec<String>,
    pub default: Option<String>,
    /// Names to show for allowed values (value → name), e.g. graphics `1` → "fancy".
    pub labels: Vec<(String, String)>,
    /// The allowed values are steps along a scale: a slider rather than a list of choices.
    pub steps: bool,
    /// Show switches as "on"/"off" rather than the file's own "true"/"false".
    pub on_off: bool,
    /// A Minecraft key binding (`key.keyboard.w`): set by pressing the key.
    pub binding: bool,
    /// Choosing a value sets other fields too (a shader pack's profiles): value → (field key, its value, its
    /// default), the fields' values as written.
    pub presets: Vec<(String, Vec<(String, String, Option<String>)>)>,
}

impl Hint {
    /// What to show for value `v`.
    pub fn name_of<'a>(&'a self, v: &'a str) -> &'a str {
        self.labels.iter().find(|(x, _)| x == v).map_or(v, |(_, n)| n.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    /// Tables or objects it's inside, outermost first.
    pub section: Vec<String>,
    pub key: String,
    /// A readable name to show instead of the key, where one is known.
    pub label: Option<String>,
    pub value: Value,
    /// Where the value sits in the text.
    pub span: Range<usize>,
    /// Where each item of a list sits, so items an edit leaves alone keep their exact text.
    pub item_spans: Vec<Range<usize>>,
    /// Its comment, with the range/allowed/default lines taken out into `hint`.
    pub doc: String,
    pub hint: Hint,
}

#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub fields: Vec<Field>,
    /// Section path → its comment.
    pub section_docs: Vec<(Vec<String>, String)>,
}

impl Doc {
    pub fn section_doc(&self, path: &[String]) -> Option<&str> {
        self.section_docs.iter().find(|(p, _)| p == path).map(|(_, d)| d.as_str()).filter(|d| !d.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    /// 1-based.
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, column {}: {}", self.line, self.col, self.message)
    }
}

fn line_col(text: &str, at: usize) -> (usize, usize) {
    let at = at.min(text.len());
    let before = &text[..text.floor_char_boundary(at)];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

fn error_at(text: &str, at: usize, message: impl Into<String>) -> ParseError {
    let (line, col) = line_col(text, at);
    ParseError { message: message.into(), line, col }
}

pub fn parse(format: Format, text: &str) -> Result<Doc, ParseError> {
    match format {
        Format::Toml => toml_doc(text),
        Format::Json | Format::Json5 => json::doc(text),
        Format::Options => Ok(crate::vanilla_options::doc(text)),
        Format::Text | Format::ShaderSettings => Ok(Doc::default()),
    }
}

/// [`parse`], with a shader pack's options (`base`) for its settings file.
pub fn parse_with(format: Format, base: Option<&Doc>, text: &str) -> Result<Doc, ParseError> {
    match (format, base) {
        (Format::ShaderSettings, Some(b)) => Ok(crate::shader_config::doc(b, text)),
        _ => parse(format, text),
    }
}

/// Comment text without its markers, blank edges trimmed.
fn clean_comment(raw: &str) -> String {
    let lines: Vec<String> = raw
        .lines()
        .map(|l| {
            let l = l.trim();
            let l = l.strip_prefix("//").or_else(|| l.strip_prefix('#')).unwrap_or(l);
            let l = l.strip_prefix("/*").unwrap_or(l);
            let l = l.strip_suffix("*/").unwrap_or(l);
            let l = l.trim();
            let l = l.strip_prefix('*').map(str::trim).unwrap_or(l);
            // Forge's configs separate entries with a lone "." line.
            if l == "." { String::new() } else { l.to_string() }
        })
        .collect();
    let start = lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.is_empty()).map_or(start, |e| e + 1);
    lines[start..end].join("\n")
}

fn number(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches(['f', 'F', 'd', 'D', 'L']);
    s.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Split a comment into its description and what it says about the values: Forge's
/// `Range: 1 ~ 64`, `Allowed Values: A, B` and `Default: 16`, and the like.
pub fn read_hint(comment: &str) -> (String, Hint) {
    let mut hint = Hint::default();
    let mut doc = Vec::new();
    for line in comment.lines() {
        let lower = line.to_ascii_lowercase();
        let after = |prefix: &str| lower.starts_with(prefix).then(|| line[prefix.len()..].trim());
        if let Some(r) = after("range:") {
            let r = r.trim_matches(['[', ']', '(', ')']);
            if let Some((a, b)) = r.split_once('~').or_else(|| r.split_once(" to ")) {
                hint.min = number(a);
                hint.max = number(b);
            } else if let Some(a) = r.strip_prefix(">=").or_else(|| r.strip_prefix('>')) {
                hint.min = number(a);
            } else if let Some(b) = r.strip_prefix("<=").or_else(|| r.strip_prefix('<')) {
                hint.max = number(b);
            } else {
                doc.push(line);
            }
        } else if let Some(v) = after("allowed values:").or_else(|| after("valid values:")).or_else(|| after("possible values:")) {
            hint.allowed = v.split(',').map(|s| s.trim().trim_matches(['"', '\'']).to_string()).filter(|s| !s.is_empty()).collect();
        } else if let Some(d) = after("default:").or_else(|| after("default value:")) {
            hint.default = Some(d.trim_matches(['"', '\'']).to_string());
        } else {
            doc.push(line);
        }
    }
    (doc.join("\n").trim().to_string(), hint)
}

// -- TOML -----------------------------------------------------------------------

fn toml_doc(text: &str) -> Result<Doc, ParseError> {
    let doc = toml_edit::Document::parse(text.to_string()).map_err(|e| error_at(text, e.span().map_or(0, |s| s.start), e.message()))?;
    let mut out = Doc::default();
    toml_table(doc.as_table(), text, &mut Vec::new(), "", &mut out);
    Ok(out)
}

fn raw_text(r: Option<&toml_edit::RawString>, text: &str) -> String {
    r.and_then(|r| r.as_str().map(String::from).or_else(|| r.span().and_then(|s| text.get(s)).map(String::from))).unwrap_or_default()
}

fn toml_table(t: &toml_edit::Table, text: &str, path: &mut Vec<String>, dotted: &str, out: &mut Doc) {
    let mut subs: Vec<(&str, &toml_edit::Table)> = Vec::new();
    for (k, item) in t.iter() {
        let Some((key, _)) = t.get_key_value(k) else { continue };
        let name = format!("{dotted}{k}");
        match item {
            toml_edit::Item::Value(v) => {
                let comment = clean_comment(&raw_text(key.leaf_decor().prefix(), text));
                let (doc, hint) = read_hint(&comment);
                if let Some(span) = v.span() {
                    let item_spans = v.as_array().map(|a| a.iter().filter_map(|i| i.span()).collect()).unwrap_or_default();
                    out.fields.push(Field { section: path.clone(), key: name, label: None, value: toml_value(v, text), span, item_spans, doc, hint });
                }
            }
            // `a.b = 1` inside a table: keys of this table, not a section of their own.
            toml_edit::Item::Table(sub) if sub.is_dotted() => toml_table(sub, text, path, &format!("{name}."), out),
            toml_edit::Item::Table(sub) => subs.push((k, sub)),
            // `[[name]]` tables: listed so they aren't missed, edited as text.
            toml_edit::Item::ArrayOfTables(a) => {
                let comment = clean_comment(&raw_text(key.leaf_decor().prefix(), text));
                let (doc, hint) = read_hint(&comment);
                out.fields.push(Field { section: path.clone(), key: name, label: None, value: Value::Other(format!("{} [[tables]]", a.len())), span: 0..0, item_spans: Vec::new(), doc, hint });
            }
            toml_edit::Item::None => {}
        }
    }
    // Sections in the order they appear.
    subs.sort_by_key(|(_, s)| s.position().unwrap_or(isize::MAX));
    for (k, sub) in subs {
        path.push(format!("{dotted}{k}"));
        let comment = clean_comment(&raw_text(sub.decor().prefix(), text));
        out.section_docs.push((path.clone(), comment));
        toml_table(sub, text, path, "", out);
        path.pop();
    }
}

fn toml_scalar(v: &toml_edit::Value) -> Option<Scalar> {
    Some(match v {
        toml_edit::Value::Boolean(b) => Scalar::Bool(*b.value()),
        toml_edit::Value::Integer(i) => Scalar::Int(*i.value()),
        toml_edit::Value::Float(f) => Scalar::Float(*f.value()),
        toml_edit::Value::String(s) => Scalar::Str(s.value().clone()),
        _ => return None,
    })
}

fn toml_value(v: &toml_edit::Value, text: &str) -> Value {
    if let Some(s) = toml_scalar(v) {
        return Value::Scalar(s);
    }
    let raw = v.span().and_then(|s| text.get(s)).unwrap_or_default().to_string();
    match v {
        toml_edit::Value::Array(a) => {
            let items: Vec<Scalar> = a.iter().filter_map(toml_scalar).collect();
            let editable = items.len() == a.len() && !has_comment(Format::Toml, &raw);
            Value::List { items, editable }
        }
        _ => Value::Other(raw),
    }
}

/// Whether `raw` (a value's text) holds a comment, outside its strings.
fn has_comment(format: Format, raw: &str) -> bool {
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    // TOML's literal strings have no escapes.
                    if b[i] == b'\\' && !(format == Format::Toml && q == b'\'') {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'#' if format == Format::Toml => return true,
            b'/' if format != Format::Toml && matches!(b.get(i + 1), Some(b'/' | b'*')) => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

// -- writing values -------------------------------------------------------------

pub fn float_text(f: f64) -> String {
    let s = f.to_string();
    if f.is_finite() && !s.contains(['.', 'e', 'E']) { format!("{s}.0") } else { s }
}

fn escape(s: &str, json: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&if json { format!("\\u{:04x}", c as u32) } else { format!("\\u{:04X}", c as u32) }),
            c => out.push(c),
        }
    }
    out
}

/// A value's text, in the style of `old` (the text it replaces) where that's free:
/// single quotes stay single, floats stay floats.
pub fn scalar_text(format: Format, s: &Scalar, old: &str) -> String {
    match s {
        Scalar::Bool(b) => b.to_string(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Float(f) if !f.is_finite() => match format {
            Format::Toml => (if f.is_nan() { "nan" } else if *f > 0.0 { "inf" } else { "-inf" }).into(),
            _ => (if f.is_nan() { "NaN" } else if *f > 0.0 { "Infinity" } else { "-Infinity" }).into(),
        },
        Scalar::Float(f) => float_text(*f),
        // options.txt quotes some strings and not others: keep each as it was.
        Scalar::Str(v) if format == Format::Options => {
            if old.starts_with('"') {
                format!("\"{}\"", escape(v, true))
            } else {
                v.clone()
            }
        }
        Scalar::Str(v) => {
            let single = old.starts_with('\'') && !old.starts_with("'''") && format != Format::Json;
            if single && !v.contains(['\'', '\n', '\r']) && (format == Format::Toml || !v.contains('\\')) {
                format!("'{v}'")
            } else {
                format!("\"{}\"", escape(v, format != Format::Toml))
            }
        }
    }
}

/// The same value, to the bit for floats (so `-0.0` isn't `0.0` and `NaN` is itself).
fn same(a: &Scalar, b: &Scalar) -> bool {
    match (a, b) {
        (Scalar::Float(x), Scalar::Float(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// A list's text, laid out like `old` (one line or one item per line). `kept` is the old list's
/// items with their text: an item still there is written exactly as it was (`0x10`, `1_000`,
/// `'\x41'`), only new or changed ones are written afresh.
pub fn list_text(format: Format, items: &[Scalar], old: &str, kept: &[(&Scalar, &str)]) -> String {
    let first_old = old.trim_start_matches('[').trim_start();
    let mut used = vec![false; kept.len()];
    let texts: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(n, it)| {
            // The item at the same place first, else the first unused one like it.
            let at = (n < kept.len() && !used[n] && same(kept[n].0, it)).then_some(n).or_else(|| (0..kept.len()).find(|&k| !used[k] && same(kept[k].0, it)));
            match at {
                Some(k) => {
                    used[k] = true;
                    kept[k].1.to_string()
                }
                None => scalar_text(format, it, first_old),
            }
        })
        .collect();
    if items.is_empty() {
        return "[]".into();
    }
    if !old.contains('\n') {
        let sep = if old.contains(", ") || !old.contains(',') { ", " } else { "," };
        let pad = if old.starts_with("[ ") { " " } else { "" };
        return format!("[{pad}{}{pad}]", texts.join(sep));
    }
    // One per line: the first item's indent, the closing bracket's, and a trailing
    // comma if the old list had one.
    let lines: Vec<&str> = old.lines().collect();
    let indent: String = lines.iter().skip(1).find(|l| !l.trim().is_empty() && l.trim() != "]").map(|l| l[..l.len() - l.trim_start().len()].to_string()).unwrap_or_else(|| "  ".into());
    let close = lines.last().map(|l| l[..l.len() - l.trim_start().len()].to_string()).unwrap_or_default();
    let trailing = old.trim_end().trim_end_matches(']').trim_end().ends_with(',');
    let mut s = String::from("[\n");
    for (n, it) in texts.iter().enumerate() {
        s.push_str(&indent);
        s.push_str(it);
        if n + 1 < items.len() || trailing {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str(&close);
    s.push(']');
    s
}

/// The text with `field` set to `new`; only the value's own bytes change.
pub fn set(format: Format, text: &str, field: &Field, new: &Value) -> anyhow::Result<String> {
    if format == Format::ShaderSettings {
        return crate::shader_config::set(text, field, new);
    }
    let old = text.get(field.span.clone()).context("the file changed under the editor")?;
    let lit = match new {
        Value::Scalar(s) => scalar_text(format, s, old),
        Value::List { items, .. } => {
            let Value::List { items: was, editable: true } = &field.value else { bail!("{} can only be edited as text", field.key) };
            let lexemes: Vec<&str> = field.item_spans.iter().filter_map(|s| text.get(s.clone())).collect();
            let kept: Vec<(&Scalar, &str)> = if lexemes.len() == was.len() { was.iter().zip(lexemes).collect() } else { Vec::new() };
            list_text(format, items, old, &kept)
        }
        Value::Other(_) => bail!("{} can only be edited as text", field.key),
    };
    let mut out = String::with_capacity(text.len() + lit.len());
    out.push_str(&text[..field.span.start]);
    out.push_str(&lit);
    out.push_str(&text[field.span.end..]);
    check(format, &out, field, new)?;
    Ok(out)
}

/// That `text` still parses and reads `new` back where `field` was: a value written wrongly (a
/// character a TOML literal string can't hold, a number rounded on the way) is refused, not saved.
fn check(format: Format, text: &str, field: &Field, new: &Value) -> anyhow::Result<()> {
    if !matches!(format, Format::Toml | Format::Json | Format::Json5) {
        return Ok(());
    }
    let doc = parse(format, text).map_err(|e| anyhow::anyhow!("{} can't be written that way ({e}); edit it as text", field.key))?;
    let back = doc.fields.iter().find(|f| f.span.start == field.span.start && f.key == field.key).map(|f| &f.value);
    let ok = match (new, back) {
        (Value::Scalar(a), Some(Value::Scalar(b))) => same(a, b),
        (Value::List { items: a, .. }, Some(Value::List { items: b, .. })) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b)),
        _ => false,
    };
    if !ok {
        bail!("{} can't be written that way; edit it as text", field.key);
    }
    Ok(())
}

// -- JSON / JSON5 ---------------------------------------------------------------

mod json {
    //! A forgiving JSON5 reader that keeps where each value is and the comments before
    //! each key. Plain JSON is JSON5, so it reads both.

    use super::*;

    enum Node {
        Object(Vec<Member>),
        Array(Vec<Node>, Range<usize>),
        Scalar(Scalar, Range<usize>),
        Null(Range<usize>),
        /// A value the fields can't hold exactly (an integer past `i64`): shown, edited only as text.
        Raw(Range<usize>),
    }

    impl Node {
        fn span(&self) -> Option<Range<usize>> {
            match self {
                Node::Object(_) => None,
                Node::Array(_, s) | Node::Scalar(_, s) | Node::Null(s) | Node::Raw(s) => Some(s.clone()),
            }
        }
    }

    struct Member {
        key: String,
        comment: String,
        value: Node,
    }

    struct P<'a> {
        s: &'a str,
        b: &'a [u8],
        i: usize,
        /// Comments seen since the last key, for the next one.
        pending: String,
    }

    impl<'a> P<'a> {
        fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
            Err(error_at(self.s, self.i, msg))
        }

        /// Skip whitespace and comments, keeping the comments.
        fn ws(&mut self) -> Result<(), ParseError> {
            loop {
                match self.b.get(self.i) {
                    Some(c) if c.is_ascii_whitespace() => self.i += 1,
                    Some(b'/') if self.b.get(self.i + 1) == Some(&b'/') => {
                        let end = self.s[self.i..].find('\n').map_or(self.b.len(), |n| self.i + n);
                        self.pending.push_str(&self.s[self.i..end]);
                        self.pending.push('\n');
                        self.i = end;
                    }
                    Some(b'/') if self.b.get(self.i + 1) == Some(&b'*') => {
                        let Some(n) = self.s[self.i + 2..].find("*/") else { return self.err("a /* comment that never ends") };
                        let end = self.i + 2 + n + 2;
                        self.pending.push_str(&self.s[self.i..end]);
                        self.pending.push('\n');
                        self.i = end;
                    }
                    // A byte-order mark at the very start.
                    Some(0xEF) if self.i == 0 && self.s.starts_with('\u{feff}') => self.i += 3,
                    _ => return Ok(()),
                }
            }
        }

        /// A comment on the same line as what came before it (`"a": 1, // why`).
        fn same_line_comment(&mut self) -> Option<String> {
            let mut j = self.i;
            while matches!(self.b.get(j), Some(b' ' | b'\t')) {
                j += 1;
            }
            if self.b.get(j) == Some(&b'/') && self.b.get(j + 1) == Some(&b'/') {
                let end = self.s[j..].find('\n').map_or(self.b.len(), |n| j + n);
                let c = self.s[j..end].to_string();
                self.i = end;
                return Some(c);
            }
            None
        }

        fn value(&mut self, depth: usize) -> Result<Node, ParseError> {
            if depth > 64 {
                return self.err("nested too deeply");
            }
            self.ws()?;
            let start = self.i;
            match self.b.get(self.i) {
                None => self.err("the file ends where a value should be"),
                Some(b'{') => self.object(depth),
                Some(b'[') => {
                    self.i += 1;
                    let mut items = Vec::new();
                    loop {
                        self.ws()?;
                        self.pending.clear();
                        if self.b.get(self.i) == Some(&b']') {
                            self.i += 1;
                            break;
                        }
                        items.push(self.value(depth + 1)?);
                        self.ws()?;
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b']') => {}
                            _ => return self.err("expected , or ] in a list"),
                        }
                    }
                    self.pending.clear();
                    Ok(Node::Array(items, start..self.i))
                }
                Some(q @ (b'"' | b'\'')) => {
                    let q = *q;
                    let (s, exact) = self.string(q)?;
                    Ok(if exact { Node::Scalar(Scalar::Str(s), start..self.i) } else { Node::Raw(start..self.i) })
                }
                Some(_) => {
                    let word_end = self.s[self.i..].find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))).map_or(self.b.len(), |n| self.i + n);
                    let word = &self.s[self.i..word_end];
                    if word.is_empty() {
                        return self.err(format!("unexpected {:?}", self.s[self.i..].chars().next().unwrap_or(' ')));
                    }
                    let node = match word {
                        "true" => Node::Scalar(Scalar::Bool(true), start..word_end),
                        "false" => Node::Scalar(Scalar::Bool(false), start..word_end),
                        "null" => Node::Null(start..word_end),
                        w => match num(w).ok_or_else(|| error_at(self.s, start, format!("{w:?} isn't a value")))? {
                            Some(n) => Node::Scalar(n, start..word_end),
                            None => Node::Raw(start..word_end),
                        },
                    };
                    self.i = word_end;
                    Ok(node)
                }
            }
        }

        fn object(&mut self, depth: usize) -> Result<Node, ParseError> {
            self.i += 1;
            let mut members = Vec::new();
            loop {
                self.pending.clear();
                self.ws()?;
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    self.pending.clear();
                    return Ok(Node::Object(members));
                }
                let comment = std::mem::take(&mut self.pending);
                let key = match self.b.get(self.i) {
                    Some(q @ (b'"' | b'\'')) => {
                        let q = *q;
                        self.string(q)?.0
                    }
                    Some(c) if c.is_ascii_alphabetic() || matches!(c, b'_' | b'$') || *c >= 0x80 => {
                        let end = self.s[self.i..].find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$'))).map_or(self.b.len(), |n| self.i + n);
                        let k = self.s[self.i..end].to_string();
                        self.i = end;
                        k
                    }
                    None => return self.err("the file ends inside { }"),
                    _ => return self.err("expected a key"),
                };
                self.ws()?;
                if self.b.get(self.i) != Some(&b':') {
                    return self.err(format!("expected : after {key:?}"));
                }
                self.i += 1;
                self.pending.clear();
                let value = self.value(depth + 1)?;
                let mut comment = comment;
                // `"a": 1 // why`, with no comma after.
                if let Some(c) = self.same_line_comment() {
                    comment.push('\n');
                    comment.push_str(&c);
                }
                members.push(Member { key, comment, value });
                self.ws()?;
                match self.b.get(self.i) {
                    Some(b',') => {
                        self.i += 1;
                        if let Some(c) = self.same_line_comment()
                            && let Some(m) = members.last_mut()
                        {
                            m.comment.push('\n');
                            m.comment.push_str(&c);
                        }
                    }
                    Some(b'}') => {}
                    _ => return self.err(format!("expected , or }} after {:?}", members.last().map_or("", |m| m.key.as_str()))),
                }
            }
        }

        /// A string's text, and whether that is exactly what it holds: false for escapes a Rust
        /// string can't hold (a lone `\uD800`) or that JSON5 doesn't allow (`\1`), so the value is
        /// edited only as text rather than written back as something else.
        fn string(&mut self, q: u8) -> Result<(String, bool), ParseError> {
            let start = self.i;
            self.i += 1;
            let mut out = String::new();
            let mut exact = true;
            loop {
                let Some(&c) = self.b.get(self.i) else {
                    self.i = start;
                    return self.err("a string that never ends");
                };
                if c == q {
                    self.i += 1;
                    return Ok((out, exact));
                }
                if c == b'\\' {
                    let Some(&e) = self.b.get(self.i + 1) else { return self.err("a string that never ends") };
                    self.i += 2;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'v' => out.push('\u{b}'),
                        // `\0` is NUL only when no digit follows; `\1`…`\9` aren't JSON5.
                        b'0' if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) => out.push('\0'),
                        b'0'..=b'9' => exact = false,
                        // A backslash before a line ending continues the string: LF, CR, CRLF.
                        b'\n' => {}
                        b'\r' => {
                            if self.b.get(self.i) == Some(&b'\n') {
                                self.i += 1;
                            }
                        }
                        b'x' => {
                            let hex = self.s.get(self.i..self.i + 2).filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit())).and_then(|h| u8::from_str_radix(h, 16).ok());
                            let Some(code) = hex else { return self.err("a bad \\x escape") };
                            self.i += 2;
                            out.push(char::from(code));
                        }
                        b'u' => {
                            let hex4 = |at: usize| self.s.get(at..at + 4).filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit())).and_then(|h| u32::from_str_radix(h, 16).ok());
                            let Some(mut code) = hex4(self.i) else { return self.err("a bad \\u escape") };
                            self.i += 4;
                            if (0xD800..0xDC00).contains(&code)
                                && self.s.get(self.i..self.i + 2) == Some("\\u")
                                && let Some(lo) = hex4(self.i + 2).filter(|lo| (0xDC00..0xE000).contains(lo))
                            {
                                code = 0x10000 + ((code - 0xD800) << 10) + (lo - 0xDC00);
                                self.i += 6;
                            }
                            // A lone surrogate has no char: shown, but edited only as text.
                            match char::from_u32(code) {
                                Some(ch) => out.push(ch),
                                None => {
                                    out.push('\u{fffd}');
                                    exact = false;
                                }
                            }
                        }
                        _ => {
                            // \" \' \\ \/ and any other character stand for themselves, but a
                            // backslash before U+2028 or U+2029 continues the string, like one
                            // before a line ending.
                            let ch = self.s[self.i - 1..].chars().next().unwrap_or('?');
                            self.i += ch.len_utf8() - 1;
                            if !matches!(ch, '\u{2028}' | '\u{2029}') {
                                out.push(ch);
                            }
                        }
                    }
                    continue;
                }
                let ch = self.s[self.i..].chars().next().unwrap_or('?');
                out.push(ch);
                self.i += ch.len_utf8();
            }
        }
    }

    /// A number's value: `None` if `w` isn't a number, `Some(None)` if it is one the fields can't hold
    /// exactly (an integer past `i64`, a finite number past `f64`), so it isn't rounded on its way back.
    fn num(w: &str) -> Option<Option<Scalar>> {
        let (neg, body) = match w.as_bytes().first() {
            Some(b'-') => (true, &w[1..]),
            Some(b'+') => (false, &w[1..]),
            _ => (false, w),
        };
        let sign = if neg { -1.0 } else { 1.0 };
        match body {
            "Infinity" => return Some(Some(Scalar::Float(sign * f64::INFINITY))),
            "NaN" => return Some(Some(Scalar::Float(f64::NAN))),
            _ => {}
        }
        if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            if hex.is_empty() || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let v = i128::from_str_radix(hex, 16).ok().map(|v| if neg { -v } else { v });
            return Some(v.and_then(|v| i64::try_from(v).ok()).map(Scalar::Int));
        }
        if !body.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
            return None;
        }
        if !body.contains(['.', 'e', 'E']) {
            if !body.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            return Some(w.trim_start_matches('+').parse::<i64>().ok().map(Scalar::Int));
        }
        let f: f64 = body.parse().ok().or_else(|| format!("{body}0").parse().ok())?;
        Some(f.is_finite().then_some(Scalar::Float(sign * f)))
    }

    pub(super) fn doc(text: &str) -> Result<Doc, ParseError> {
        let mut p = P { s: text, b: text.as_bytes(), i: 0, pending: String::new() };
        let root = p.value(0)?;
        p.pending.clear();
        p.ws()?;
        if p.i < p.b.len() {
            return p.err("more text after the end of the file's value");
        }
        let mut out = Doc::default();
        if let Node::Object(members) = root {
            walk(members, text, &mut Vec::new(), &mut out);
        }
        Ok(out)
    }

    fn walk(members: Vec<Member>, text: &str, path: &mut Vec<String>, out: &mut Doc) {
        let mut subs = Vec::new();
        for m in members {
            let comment = clean_comment(&m.comment);
            let (doc, hint) = read_hint(&comment);
            let mut item_spans = Vec::new();
            let (value, span) = match m.value {
                Node::Object(inner) => {
                    subs.push((m.key, comment, inner));
                    continue;
                }
                Node::Scalar(s, span) => (Value::Scalar(s), span),
                Node::Null(span) => (Value::Other("null".into()), span),
                Node::Raw(span) => (Value::Other(text[span.clone()].to_string()), span),
                Node::Array(items, span) => {
                    item_spans = items.iter().filter_map(Node::span).collect();
                    let all = items.len();
                    let scalars: Vec<Scalar> = items.into_iter().filter_map(|n| if let Node::Scalar(s, _) = n { Some(s) } else { None }).collect();
                    let editable = scalars.len() == all && !has_comment(Format::Json5, &text[span.clone()]);
                    if scalars.len() == all {
                        (Value::List { items: scalars, editable }, span)
                    } else {
                        (Value::Other(text[span.clone()].to_string()), span)
                    }
                }
            };
            out.fields.push(Field { section: path.clone(), key: m.key, label: None, value, span, item_spans, doc, hint });
        }
        // Objects inside become sections, after this one's own keys, as in TOML.
        for (key, comment, inner) in subs {
            path.push(key);
            out.section_docs.push((path.clone(), comment));
            walk(inner, text, path, out);
            path.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORGE: &str = "# Client settings\n\n[client]\n\t#Range: 1 ~ 64\n\tmaxStack = 16 # trailing\n\t#How shiny things look.\n\t#Allowed Values: LOW, MEDIUM, HIGH\n\tquality = \"HIGH\"\n\tnames = [ \"a\", 'b' ]\n\n\t#Rendering stuff\n\t[client.rendering]\n\t\t#Default: 70.0\n\t\t#Range: 30.0 ~ 110.0\n\t\tfov = 70.5\n\t\tsmooth.lighting = true\n";

    const FABRIC: &str = "{\n  // Shows the minimap.\n  \"enabled\": true,\n  'zoom': 2.5, // how far out\n  scale: 3,\n  /* Where it sits.\n   * Allowed values: TOP_LEFT, TOP_RIGHT */\n  \"corner\": \"TOP_LEFT\",\n  \"colors\": [\n    \"red\",\n    \"blue\",\n  ],\n  \"advanced\": {\n    \"radius\": 0x10,\n    \"label\": \"a \\\"quoted\\\" \\u00e9\"\n  },\n}\n";

    fn field<'a>(d: &'a Doc, key: &str) -> &'a Field {
        d.fields.iter().find(|f| f.key == key).unwrap_or_else(|| panic!("no {key} in {:?}", d.fields.iter().map(|f| &f.key).collect::<Vec<_>>()))
    }

    #[test]
    fn forge_spacer_dots_are_not_descriptions() {
        assert_eq!(clean_comment("#.\n#The vision range\n#."), "The vision range");
        assert_eq!(clean_comment("#."), "");
    }

    #[test]
    fn toml_fields_keep_their_comments_and_hints() {
        let d = parse(Format::Toml, FORGE).unwrap();
        let keys: Vec<&str> = d.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["maxStack", "quality", "names", "fov", "smooth.lighting"]);
        let max = field(&d, "maxStack");
        assert_eq!(max.section, ["client"]);
        assert_eq!((max.hint.min, max.hint.max), (Some(1.0), Some(64.0)));
        assert_eq!(max.value, Value::Scalar(Scalar::Int(16)));
        let q = field(&d, "quality");
        assert_eq!(q.doc, "How shiny things look.");
        assert_eq!(q.hint.allowed, ["LOW", "MEDIUM", "HIGH"]);
        let fov = field(&d, "fov");
        assert_eq!(fov.section, ["client", "rendering"]);
        assert_eq!(fov.hint.default.as_deref(), Some("70.0"));
        assert_eq!(fov.hint.min, Some(30.0));
        assert_eq!(field(&d, "smooth.lighting").section, ["client", "rendering"]);
        assert_eq!(d.section_doc(&["client".into(), "rendering".into()]), Some("Rendering stuff"));
        assert_eq!(d.section_doc(&["client".into()]), Some("Client settings"));
    }

    /// Changing values touches only their bytes: comments, tabs and quotes stay.
    #[test]
    fn toml_edits_round_trip_byte_for_byte() {
        let d = parse(Format::Toml, FORGE).unwrap();
        let t = set(Format::Toml, FORGE, field(&d, "maxStack"), &Value::Scalar(Scalar::Int(32))).unwrap();
        let d = parse(Format::Toml, &t).unwrap();
        let t = set(Format::Toml, &t, field(&d, "quality"), &Value::Scalar(Scalar::Str("LOW".into()))).unwrap();
        let d = parse(Format::Toml, &t).unwrap();
        let t = set(Format::Toml, &t, field(&d, "fov"), &Value::Scalar(Scalar::Float(90.0))).unwrap();
        let d = parse(Format::Toml, &t).unwrap();
        let t = set(Format::Toml, &t, field(&d, "names"), &Value::List { items: vec![Scalar::Str("a".into()), Scalar::Str("it's".into())], editable: true }).unwrap();
        let want = FORGE.replace("maxStack = 16", "maxStack = 32").replace("\"HIGH\"\n", "\"LOW\"\n").replace("70.5", "90.0").replace("[ \"a\", 'b' ]", "[ \"a\", \"it's\" ]");
        assert_eq!(t, want);
        // Back to the start gives the very same file.
        let d = parse(Format::Toml, &t).unwrap();
        let back = set(Format::Toml, &t, field(&d, "maxStack"), &Value::Scalar(Scalar::Int(16))).unwrap();
        assert_eq!(back, want.replace("maxStack = 32", "maxStack = 16"));
    }

    #[test]
    fn json5_fields_keep_their_comments() {
        let d = parse(Format::Json5, FABRIC).unwrap();
        assert_eq!(field(&d, "enabled").doc, "Shows the minimap.");
        assert_eq!(field(&d, "zoom").doc, "how far out");
        assert_eq!(field(&d, "zoom").value, Value::Scalar(Scalar::Float(2.5)));
        assert_eq!(field(&d, "scale").value, Value::Scalar(Scalar::Int(3)));
        let corner = field(&d, "corner");
        assert_eq!(corner.doc, "Where it sits.");
        assert_eq!(corner.hint.allowed, ["TOP_LEFT", "TOP_RIGHT"]);
        assert_eq!(field(&d, "radius").value, Value::Scalar(Scalar::Int(16)));
        assert_eq!(field(&d, "radius").section, ["advanced"]);
        assert_eq!(field(&d, "label").value, Value::Scalar(Scalar::Str("a \"quoted\" é".into())));
        assert!(matches!(&field(&d, "colors").value, Value::List { items, editable: true } if items.len() == 2));
    }

    #[test]
    fn json5_edits_round_trip_byte_for_byte() {
        let d = parse(Format::Json5, FABRIC).unwrap();
        let t = set(Format::Json5, FABRIC, field(&d, "enabled"), &Value::Scalar(Scalar::Bool(false))).unwrap();
        let d = parse(Format::Json5, &t).unwrap();
        let t = set(Format::Json5, &t, field(&d, "zoom"), &Value::Scalar(Scalar::Float(3.0))).unwrap();
        let d = parse(Format::Json5, &t).unwrap();
        let t = set(Format::Json5, &t, field(&d, "colors"), &Value::List { items: vec![Scalar::Str("green".into())], editable: true }).unwrap();
        let d = parse(Format::Json5, &t).unwrap();
        let t = set(Format::Json5, &t, field(&d, "label"), &Value::Scalar(Scalar::Str("new \"one\"\n".into()))).unwrap();
        let want = FABRIC
            .replace("\"enabled\": true", "\"enabled\": false")
            .replace("'zoom': 2.5", "'zoom': 3.0")
            .replace("[\n    \"red\",\n    \"blue\",\n  ]", "[\n    \"green\",\n  ]")
            .replace("\"a \\\"quoted\\\" \\u00e9\"", "\"new \\\"one\\\"\\n\"");
        assert_eq!(t, want);
        assert!(parse(Format::Json5, &t).is_ok());
    }

    /// Editing one item of a list leaves the others' text alone, and a number the fields can't hold
    /// exactly (past `i64`) is shown as text, never rounded through a float.
    #[test]
    fn untouched_list_items_keep_their_exact_text() {
        let text = r#"{"values": [18446744073709551615, 1], "seed": 18446744073709551615, "mask": 0xFFFFFFFFFFFFFFFF, "odd": [0x10, 1.50, -0, '\x41', 2]}"#;
        let d = parse(Format::Json5, text).unwrap();
        // The big integers aren't numbers the fields could write back: text only, and so is their list.
        assert_eq!(field(&d, "seed").value, Value::Other("18446744073709551615".into()));
        assert_eq!(field(&d, "mask").value, Value::Other("0xFFFFFFFFFFFFFFFF".into()));
        assert!(matches!(field(&d, "values").value, Value::Other(_)));
        assert!(set(Format::Json5, text, field(&d, "values"), &Value::List { items: vec![Scalar::Int(1), Scalar::Int(2)], editable: true }).is_err());
        // Changing the last item keeps every other item's own text.
        let Value::List { items, .. } = &field(&d, "odd").value else { panic!() };
        let mut items = items.clone();
        *items.last_mut().unwrap() = Scalar::Int(3);
        let t = set(Format::Json5, text, field(&d, "odd"), &Value::List { items: items.clone(), editable: true }).unwrap();
        assert_eq!(t, text.replace(r"'\x41', 2]", r"'\x41', 3]"));
        // Taking one out keeps the rest as written.
        items.remove(1);
        let t = set(Format::Json5, text, field(&d, "odd"), &Value::List { items, editable: true }).unwrap();
        assert_eq!(t, text.replace("[0x10, 1.50, -0, ", "[0x10, -0, ").replace(", 2]", ", 3]"));
        // In TOML too: hex, underscores and literal strings stay as they were.
        let toml = "a = [0x10, 1_000, 'x\\y', 4]\n";
        let d = parse(Format::Toml, toml).unwrap();
        let t = set(Format::Toml, toml, field(&d, "a"), &Value::List { items: vec![Scalar::Int(16), Scalar::Int(1000), Scalar::Str("x\\y".into()), Scalar::Int(5)], editable: true }).unwrap();
        assert_eq!(t, "a = [0x10, 1_000, 'x\\y', 5]\n");
    }

    /// JSON5 strings decode to what they mean (`\x41` is `A`, a backslash before any line ending
    /// continues the line), so an edit writes back the right text; escapes that can't be held
    /// exactly leave the value text only.
    #[test]
    fn json5_strings_decode_every_escape() {
        let text = "{hex: '\\x41', lf: 'a\\\nb', cr: 'a\\\rb', crlf: 'a\\\r\nb', ls: 'a\\\u{2028}b', ps: \"a\\\u{2029}b\", nul: '\\0', pair: '\\uD83D\\uDE00', lone: '\\uD800', octal: '\\1', list: ['\\x41', 'b']}";
        let d = parse(Format::Json5, text).unwrap();
        let s = |k: &str| match &field(&d, k).value {
            Value::Scalar(Scalar::Str(s)) => s.clone(),
            v => panic!("{k} is {v:?}"),
        };
        assert_eq!(s("hex"), "A");
        for k in ["lf", "cr", "crlf", "ls", "ps"] {
            assert_eq!(s(k), "ab", "{k}");
        }
        assert_eq!(s("nul"), "\0");
        assert_eq!(s("pair"), "\u{1F600}");
        assert!(matches!(field(&d, "lone").value, Value::Other(_)));
        assert!(matches!(field(&d, "octal").value, Value::Other(_)));
        // An edit keeps the decoded value: `A` plus `!`, not `x41!`.
        let t = set(Format::Json5, text, field(&d, "hex"), &Value::Scalar(Scalar::Str(s("hex") + "!"))).unwrap();
        assert_eq!(field(&parse(Format::Json5, &t).unwrap(), "hex").value, Value::Scalar(Scalar::Str("A!".into())));
        let t = set(Format::Json5, text, field(&d, "crlf"), &Value::Scalar(Scalar::Str(s("crlf") + "c"))).unwrap();
        assert_eq!(field(&parse(Format::Json5, &t).unwrap(), "crlf").value, Value::Scalar(Scalar::Str("abc".into())));
        // Changing one item of a list of strings keeps the other's meaning.
        let t = set(Format::Json5, text, field(&d, "list"), &Value::List { items: vec![Scalar::Str("A".into()), Scalar::Str("c".into())], editable: true }).unwrap();
        assert_eq!(field(&parse(Format::Json5, &t).unwrap(), "list").value, Value::List { items: vec![Scalar::Str("A".into()), Scalar::Str("c".into())], editable: true });
    }

    #[test]
    fn strict_json_and_errors() {
        let d = parse(Format::Json, "{\"a\": {\"b\": [1, 2.5, \"x\", null]}, \"n\": null}").unwrap();
        assert!(matches!(field(&d, "b").value, Value::Other(_)));
        assert!(matches!(field(&d, "n").value, Value::Other(_)));
        let e = parse(Format::Json, "{\n  \"a\": 1\n  \"b\": 2\n}").unwrap_err();
        assert_eq!((e.line, e.col), (3, 3));
        let e = parse(Format::Toml, "a = [1,\nb = 2").unwrap_err();
        assert_eq!(e.line, 2);
        // A list with comments inside isn't rewritten from fields.
        let d = parse(Format::Toml, "a = [\n  1, # one\n  2,\n]\n").unwrap();
        assert!(matches!(field(&d, "a").value, Value::List { editable: false, .. }));
        assert!(set(Format::Toml, "a = [\n  1, # one\n  2,\n]\n", field(&d, "a"), &Value::List { items: vec![], editable: true }).is_err());
    }

    #[test]
    fn hints_read_ranges_values_and_defaults() {
        let (doc, h) = read_hint("Max players.\nRange: > 0\nDefault: 8");
        assert_eq!(doc, "Max players.");
        assert_eq!((h.min, h.max, h.default.as_deref()), (Some(0.0), None, Some("8")));
        let (_, h) = read_hint("Range: -2147483648 ~ 2147483647");
        assert_eq!(h.min, Some(-2147483648.0));
        let (_, h) = read_hint("Range: 0.0 ~ 1.7976931348623157E308");
        assert!(h.max.unwrap() > 1e300);
        let (doc, h) = read_hint("Allowed Values: A, \"B\", C\nwhat it does");
        assert_eq!(h.allowed, ["A", "B", "C"]);
        assert_eq!(doc, "what it does");
        assert_eq!(clean_comment("\t# one\n\t#\n\t# two\n"), "one\n\ntwo");
    }

    #[test]
    fn files_group_under_their_mods() {
        let f = |p: &str| ConfigFile { rel: p.into(), size: 1, modified: None };
        let files = [f("sodium-options.json"), f("create-client.toml"), f("jei/jei-client.ini"), f("xaerominimap.txt"), f("mystery.cfg"), f("fabric/indigo-renderer.properties")];
        let m = |name: &str, ids: &[&str]| ModInfo { name: name.into(), ids: ids.iter().map(|s| s.to_string()).collect(), icon: None };
        let mods = [m("Sodium", &["sodium"]), m("Create", &["create"]), m("Just Enough Items", &["jei"]), m("Xaero's Minimap", &["xaerominimap"]), m("Fabric API", &["fabric-api", "fabric-renderer-indigo"])];
        let groups = group(&files, &mods);
        let names: Vec<(&str, Vec<String>)> = groups.iter().map(|g| (g.name.as_str(), g.files.iter().map(|f| f.rel.display().to_string()).collect())).collect();
        assert_eq!(
            names,
            [
                ("Create", vec!["create-client.toml".to_string()]),
                ("Fabric API", vec!["fabric/indigo-renderer.properties".to_string()]),
                ("Just Enough Items", vec!["jei/jei-client.ini".to_string()]),
                ("Sodium", vec!["sodium-options.json".to_string()]),
                ("Xaero's Minimap", vec!["xaerominimap.txt".to_string()]),
                ("other", vec!["mystery.cfg".to_string()]),
            ]
        );
    }

    fn scratch_instance(name: &str) -> Instance {
        let mut inst = Instance::new(name, "1.21.1", crate::loaders::Loader::Vanilla);
        inst.assign_id().unwrap();
        inst
    }

    #[test]
    fn only_files_inside_config_open() {
        let inst = scratch_instance("config paths");
        let cfg = config_dir(&inst);
        std::fs::create_dir_all(cfg.join("sub")).unwrap();
        std::fs::write(cfg.join("sub/a.toml"), "a = 1").unwrap();
        std::fs::write(inst.game_dir().join("options.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(inst.game_dir().join("options.txt"), cfg.join("escape.txt")).unwrap();
        std::os::unix::fs::symlink(cfg.join("sub/a.toml"), cfg.join("inside.toml")).unwrap();
        std::os::unix::fs::symlink(inst.game_dir(), cfg.join("up")).unwrap();
        assert!(load(&inst, Path::new("sub/a.toml")).is_ok());
        assert!(load(&inst, Path::new("inside.toml")).is_ok());
        for bad in ["escape.txt", "../options.txt", "/etc/passwd", "up/options.txt", "sub/../../options.txt"] {
            assert!(load(&inst, Path::new(bad)).is_err(), "{bad} opened");
        }
        let listed: Vec<String> = list(&inst).iter().map(|f| f.rel.display().to_string()).collect();
        assert_eq!(listed, ["inside.toml", "sub/a.toml"]);
        // Too big to open.
        std::fs::write(cfg.join("big.json"), vec![b' '; MAX_OPEN as usize + 1]).unwrap();
        assert!(format!("{:#}", load(&inst, Path::new("big.json")).unwrap_err()).contains("too big"));
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn a_file_changed_on_disk_is_not_overwritten() {
        let inst = scratch_instance("config stale");
        let cfg = config_dir(&inst);
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("a.toml"), "a = 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cfg.join("a.toml"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = load(&inst, Path::new("a.toml")).unwrap();
        // The game rewrites it while it's open in the editor.
        std::fs::write(cfg.join("a.toml"), "a = 5\n").unwrap();
        match save(&loaded.path, &loaded.text, "a = 2\n", false).unwrap() {
            Saved::Changed(now) => assert_eq!(now, "a = 5\n"),
            Saved::Done(_) => panic!("overwrote the game's change"),
        }
        assert_eq!(std::fs::read_to_string(cfg.join("a.toml")).unwrap(), "a = 5\n");
        // Overwriting on purpose works, and keeps the file's permissions.
        assert!(matches!(save(&loaded.path, &loaded.text, "a = 2\n", true).unwrap(), Saved::Done(_)));
        assert_eq!(std::fs::read_to_string(cfg.join("a.toml")).unwrap(), "a = 2\n");
        assert_eq!(std::fs::metadata(cfg.join("a.toml")).unwrap().permissions().mode() & 0o777, 0o600);
        // An unchanged file saves straight away.
        assert!(matches!(save(&loaded.path, "a = 2\n", "a = 3\n", false).unwrap(), Saved::Done(_)));
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }
}
