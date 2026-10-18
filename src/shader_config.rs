//! Shader pack settings, as Iris and OptiFine keep them: a pack's options are declared in its
//! own shader code (`#define NAME value // [a b c]`, or a `#define` switched off by `//`), named
//! in its language file (`shaders/lang/en_us.lang`) and laid out in screens by
//! `shaders/shaders.properties`. What the player changed is saved beside the pack, in
//! `shaderpacks/<pack>.txt`, as `NAME=value` lines; anything at its default is left out.
//!
//! Packs are only read here, zipped or not; the one file written is that settings file.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};

use crate::instance::Instance;
use crate::mod_config::{self as mc, Doc, Field, Format, Hint, Loaded, Scalar, Value};

/// Limits on what's read from one pack (packs are a few MB of source at most).
const MAX_FILE: u64 = 4 << 20;
const MAX_TOTAL: u64 = 64 << 20;
const MAX_FILES: usize = 6000;
/// How deep screens may nest.
const MAX_DEPTH: usize = 8;

const SOURCES: &[&str] = &["glsl", "vsh", "fsh", "gsh", "csh", "tcs", "tes", "inc", "h"];

pub fn dir(inst: &Instance) -> PathBuf {
    inst.game_dir().join("shaderpacks")
}

/// A shader pack in the instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Pack {
    /// Its file or folder name in `shaderpacks/`.
    pub name: String,
    /// The pack Iris (or OptiFine) has switched on.
    pub active: bool,
}

/// Where the pack in use is chosen: Iris's `config/iris.properties` or OptiFine's `optionsshaders.txt`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Switch {
    Iris,
    OptiFine,
}

impl Switch {
    fn path(self, inst: &Instance) -> PathBuf {
        match self {
            Switch::Iris => inst.game_dir().join("config/iris.properties"),
            Switch::OptiFine => inst.game_dir().join("optionsshaders.txt"),
        }
    }
}

/// Which shader mod picks the pack here: the one whose settings file exists, else the one installed (Iris writes its
/// file on first start; writing it first is what it would do).
pub fn switch(inst: &Instance) -> Option<Switch> {
    for s in [Switch::Iris, Switch::OptiFine] {
        if s.path(inst).is_file() {
            return Some(s);
        }
    }
    let mods = inst.mods();
    let has = |word: &str| mods.iter().any(|m| m.enabled && m.name.to_ascii_lowercase().starts_with(word));
    if has("iris") || has("oculus") {
        Some(Switch::Iris)
    } else if has("optifine") || has("optifabric") {
        Some(Switch::OptiFine)
    } else {
        None
    }
}

/// Java's `.properties` escapes (Iris writes its file with them): `\uXXXX` for anything past ASCII, `\\` for `\`.
fn escape(v: &str) -> String {
    let mut out = String::new();
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04X}"));
                }
            }
        }
    }
    out
}

fn unescape(v: &str) -> String {
    let mut units: Vec<u16> = Vec::new();
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            units.extend(c.encode_utf16(&mut [0u16; 2]).iter());
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                units.extend(u16::from_str_radix(&hex, 16).ok());
            }
            Some(n) => units.extend(n.encode_utf16(&mut [0u16; 2]).iter()),
            None => {}
        }
    }
    String::from_utf16_lossy(&units)
}

/// A `.properties` value by key.
fn prop(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| l.trim().strip_prefix(key)?.trim_start().strip_prefix(['=', ':']).map(|v| unescape(v.trim())))
}

/// `text` with `key` set to `value` (escaped), its line replaced in place or added at the end.
fn set_prop(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key}={}", escape(value));
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    match lines.iter().position(|l| l.trim().strip_prefix(key).is_some_and(|r| r.trim_start().starts_with(['=', ':']))) {
        Some(i) => lines[i] = line,
        None => lines.push(line),
    }
    lines.join("\n") + "\n"
}

/// The pack switched on, from Iris's `config/iris.properties` or OptiFine's `optionsshaders.txt`.
fn active_pack(inst: &Instance) -> Option<String> {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok();
    if let Some(t) = read(Switch::Iris.path(inst)) {
        let on = prop(&t, "enableShaders").is_none_or(|v| v == "true");
        return prop(&t, "shaderPack").filter(|p| on && !p.is_empty());
    }
    let t = read(Switch::OptiFine.path(inst))?;
    prop(&t, "shaderPack").filter(|p| !p.is_empty() && p != "OFF" && p != "(internal)")
}

/// Switch shaders to pack `name`, or off (`None`), in whichever file the instance's shader mod reads. Only those
/// lines change.
pub fn use_pack(inst: &Instance, name: Option<&str>) -> anyhow::Result<()> {
    let s = switch(inst).context("there's no shader mod (Iris or OptiFine) in this instance")?;
    if let Some(n) = name {
        checked(n)?;
        if !dir(inst).join(n).exists() {
            bail!("{n} isn't in shaderpacks/ any more");
        }
    }
    let path = s.path(inst);
    let text = if path.exists() { mc::read_text(&path)? } else { String::new() };
    let text = match (s, name) {
        (Switch::Iris, Some(n)) => set_prop(&set_prop(&text, "shaderPack", n), "enableShaders", "true"),
        (Switch::Iris, None) => set_prop(&text, "enableShaders", "false"),
        (Switch::OptiFine, Some(n)) => set_prop(&text, "shaderPack", n),
        (Switch::OptiFine, None) => set_prop(&text, "shaderPack", "OFF"),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::fsutil::write_atomic(&path, text.as_bytes(), 0o644)?;
    Ok(())
}

/// The instance's shader packs: zips and folders in `shaderpacks/`, by name.
pub fn packs(inst: &Instance) -> Vec<Pack> {
    let active = active_pack(inst);
    let mut out: Vec<Pack> = std::fs::read_dir(dir(inst))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let ok = if path.is_dir() { path.join("shaders").is_dir() } else { name.to_ascii_lowercase().ends_with(".zip") };
            ok.then(|| Pack { active: active.as_deref() == Some(name.as_str()), name })
        })
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// A pack's own name, safe to join to `shaderpacks/`.
fn checked(name: &str) -> anyhow::Result<&str> {
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        bail!("{name} isn't a shader pack name");
    }
    Ok(name)
}

/// Where a pack's settings are saved.
pub fn settings_path(inst: &Instance, name: &str) -> anyhow::Result<PathBuf> {
    Ok(dir(inst).join(format!("{}.txt", checked(name)?)))
}

/// The pack's files under its `shaders/` folder, by path relative to it.
fn read_pack(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let wanted = |rel: &str| {
        let lower = rel.to_ascii_lowercase();
        lower == "shaders.properties" || (lower.starts_with("lang/") && lower.ends_with(".lang")) || Path::new(&lower).extension().is_some_and(|e| SOURCES.contains(&e.to_string_lossy().as_ref()))
    };
    let mut out = BTreeMap::new();
    let mut total = 0u64;
    let mut take = |rel: String, len: u64, r: &mut dyn Read, out: &mut BTreeMap<String, String>| -> anyhow::Result<()> {
        if len > MAX_FILE || out.len() >= MAX_FILES {
            return Ok(());
        }
        total += len;
        if total > MAX_TOTAL {
            bail!("the pack holds more source than a shader pack should");
        }
        let mut bytes = Vec::new();
        r.take(MAX_FILE).read_to_end(&mut bytes)?;
        out.insert(rel, String::from_utf8_lossy(&bytes).into_owned());
        Ok(())
    };
    if path.is_dir() {
        let root = path.join("shaders");
        let mut stack = vec![(root.clone(), 0)];
        while let Some((d, depth)) = stack.pop() {
            for e in std::fs::read_dir(&d)?.flatten() {
                let p = e.path();
                let ft = e.file_type()?;
                if ft.is_dir() && depth < 12 {
                    stack.push((p, depth + 1));
                } else if ft.is_file() {
                    let rel = p.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
                    if wanted(&rel) {
                        take(rel, e.metadata()?.len(), &mut std::fs::File::open(&p)?, &mut out)?;
                    }
                }
            }
        }
        return Ok(out);
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?).context("it isn't a readable zip")?;
    // `shaders/` at the top, or inside one folder.
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let root = if names.iter().any(|n| n.starts_with("shaders/")) {
        "shaders/".to_string()
    } else {
        names.iter().find_map(|n| n.find("/shaders/").filter(|i| !n[..*i].contains('/')).map(|i| n[..i + 9].to_string())).context("no shaders/ folder in it")?
    };
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if !f.is_file() {
            continue;
        }
        let Some(rel) = f.name()?.strip_prefix(&root).map(String::from) else { continue };
        if rel.split('/').any(|c| c == "..") || !wanted(&rel) {
            continue;
        }
        let len = f.size();
        take(rel, len, &mut f, &mut out)?;
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// A valueless `#define`, on or commented out: an option only if the code tests it.
    Switch,
    /// A `const bool` with a comment: a value the code reads, so no test is needed.
    ConstSwitch,
    Values(Vec<String>),
}

#[derive(Debug, Clone)]
struct Opt {
    name: String,
    default: String,
    kind: Kind,
    comment: String,
}

fn ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// `// [a b c] text` → (["a", "b", "c"], "text").
fn comment_values(c: &str) -> (Option<Vec<String>>, String) {
    let c = c.trim();
    if let Some(open) = c.find('[')
        && let Some(close) = c[open..].find(']')
    {
        let list = c[open + 1..open + close].split_whitespace().map(String::from).collect::<Vec<_>>();
        let rest = format!("{} {}", &c[..open], &c[open + close + 1..]).trim().to_string();
        return (Some(list).filter(|l| !l.is_empty()), rest);
    }
    (None, c.to_string())
}

/// The options declared in a pack's source, in the order first seen.
fn options(files: &BTreeMap<String, String>) -> Vec<Opt> {
    let mut found: Vec<Opt> = Vec::new();
    let mut seen = HashSet::new();
    let mut tested = HashSet::new();
    for (rel, text) in files {
        if !Path::new(rel).extension().is_some_and(|e| SOURCES.contains(&e.to_string_lossy().as_ref())) {
            continue;
        }
        for line in text.lines() {
            let l = line.trim();
            // Names the code tests: only those switches are options.
            for word in ["#ifdef", "#ifndef", "defined"] {
                for part in l.split(word).skip(1) {
                    let name: String = part.trim_start_matches([' ', '\t', '(']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if !name.is_empty() {
                        tested.insert(name);
                    }
                }
            }
            let (off, l) = match l.strip_prefix("//") {
                Some(rest) => (true, rest.trim_start()),
                None => (false, l),
            };
            let (decl, comment) = match l.find("//") {
                Some(i) => (l[..i].trim(), &l[i + 2..]),
                None => (l, ""),
            };
            let (values, text) = comment_values(comment);
            if let Some(rest) = decl.strip_prefix("#define") {
                let mut parts = rest.split_whitespace();
                let Some(name) = parts.next().filter(|n| ident(n)) else { continue };
                let value = parts.next();
                if parts.next().is_some() || seen.contains(name) {
                    continue;
                }
                let opt = match (value, values) {
                    (None, None) => Opt { name: name.into(), default: (!off).to_string(), kind: Kind::Switch, comment: text },
                    (Some(v), Some(list)) if !off => Opt { name: name.into(), default: v.into(), kind: Kind::Values(list), comment: text },
                    _ => continue,
                };
                seen.insert(opt.name.clone());
                found.push(opt);
            } else if !off && let Some(rest) = decl.strip_prefix("const ") {
                // const int shadowMapResolution = 2048; // [1024 2048 4096]
                let rest = rest.trim_end_matches(';');
                let Some((lhs, value)) = rest.split_once('=') else { continue };
                let mut lhs = lhs.split_whitespace();
                let (Some(ty), Some(name)) = (lhs.next(), lhs.next()) else { continue };
                let value = value.trim();
                if !ident(name) || seen.contains(name) || !["int", "float", "bool"].contains(&ty) {
                    continue;
                }
                let opt = match values {
                    Some(list) => Opt { name: name.into(), default: value.into(), kind: Kind::Values(list), comment: text },
                    None if ty == "bool" && !comment.is_empty() && (value == "true" || value == "false") => Opt { name: name.into(), default: value.into(), kind: Kind::ConstSwitch, comment: text },
                    None => continue,
                };
                seen.insert(opt.name.clone());
                found.push(opt);
            }
        }
    }
    found.retain(|o| o.kind != Kind::Switch || tested.contains(&o.name));
    found
}

/// `key=value` lines, with `\` continuations; `#` lines (comments and `#ifdef`s) skipped.
fn properties(text: &str) -> HashMap<String, String> {
    properties_list(text).into_iter().collect()
}

/// [`properties`] in the file's order.
fn properties_list(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for line in text.lines() {
        let l = line.trim();
        if acc.is_empty() && (l.starts_with('#') || l.starts_with('!') || l.is_empty()) {
            continue;
        }
        if let Some(cont) = l.strip_suffix('\\') {
            acc.push_str(cont);
            acc.push(' ');
            continue;
        }
        acc.push_str(l);
        if let Some((k, v)) = acc.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
        acc.clear();
    }
    out
}

/// Minecraft's `§` formatting codes taken out.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// "SHADOW_DISTANCE" → "shadow distance".
fn readable(name: &str) -> String {
    name.replace('_', " ").to_lowercase()
}

/// The pack's options as fields at their defaults, under the pack's own screens.
fn base(files: &BTreeMap<String, String>) -> Doc {
    let opts = options(files);
    let props_list = files.iter().find(|(k, _)| k.eq_ignore_ascii_case("shaders.properties")).map(|(_, t)| properties_list(t)).unwrap_or_default();
    let props: HashMap<String, String> = props_list.iter().cloned().collect();
    let lang = files.iter().find(|(k, _)| k.eq_ignore_ascii_case("lang/en_us.lang")).map(|(_, t)| properties(t)).unwrap_or_default();
    let tr = |key: String| lang.get(&key).map(|v| plain(v)).filter(|v| !v.is_empty());
    let sliders: HashSet<&str> = props.get("sliders").map(|s| s.split_whitespace().collect()).unwrap_or_default();
    let by_name: HashMap<&str, &Opt> = opts.iter().map(|o| (o.name.as_str(), o)).collect();

    // Walk the screens from the main one, placing each option under the screens that lead to it.
    let mut placed: Vec<(Vec<String>, String)> = Vec::new();
    let mut section_docs = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    fn walk<'a>(screen: &str, path: Vec<String>, ctx: &mut Walk<'a>) {
        if path.len() > MAX_DEPTH || !ctx.visited.insert(screen.to_string()) {
            return;
        }
        ctx.order.push(path.clone());
        let key = if screen.is_empty() { "screen".to_string() } else { format!("screen.{screen}") };
        let Some(list) = ctx.props.get(&key) else { return };
        for token in list.split_whitespace() {
            if let Some(sub) = token.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
                let title = (ctx.tr)(format!("screen.{sub}")).unwrap_or_else(|| readable(sub));
                let mut p = path.clone();
                p.push(title);
                if let Some(c) = (ctx.tr)(format!("screen.{sub}.comment")) {
                    ctx.docs.push((p.clone(), c));
                }
                walk(sub, p, ctx);
            } else if token == "*" {
                let rest: Vec<String> = ctx.all.iter().filter(|n| !ctx.done.contains(*n)).cloned().collect();
                for n in rest {
                    ctx.done.insert(n.clone());
                    ctx.placed.push((path.clone(), n));
                }
            } else if ctx.all.contains(&token.to_string()) && ctx.done.insert(token.to_string()) {
                ctx.placed.push((path.clone(), token.to_string()));
            }
        }
    }
    struct Walk<'a> {
        props: &'a HashMap<String, String>,
        tr: &'a dyn Fn(String) -> Option<String>,
        all: Vec<String>,
        visited: HashSet<String>,
        done: &'a mut HashSet<String>,
        placed: &'a mut Vec<(Vec<String>, String)>,
        docs: &'a mut Vec<(Vec<String>, String)>,
        /// Screens in menu order: each before the ones it leads to.
        order: &'a mut Vec<Vec<String>>,
    }
    let mut order: Vec<Vec<String>> = Vec::new();
    if props.contains_key("screen") {
        let mut w = Walk { props: &props, tr: &tr, all: opts.iter().map(|o| o.name.clone()).collect(), visited: HashSet::new(), done: &mut done, placed: &mut placed, docs: &mut section_docs, order: &mut order };
        walk("", Vec::new(), &mut w);
    } else {
        placed = opts.iter().map(|o| (Vec::new(), o.name.clone())).collect();
    }

    // Settings outside any screen first, then each screen's in menu order.
    placed.sort_by_key(|(p, _)| (!p.is_empty(), order.iter().position(|x| x == p)));
    let mut fields: Vec<Field> = placed
        .into_iter()
        .filter_map(|(section, name)| {
            let o = by_name.get(name.as_str())?;
            let mut hint = Hint { default: Some(o.default.clone()), on_off: true, ..Default::default() };
            let value = match &o.kind {
                Kind::Switch | Kind::ConstSwitch => Value::Scalar(Scalar::Bool(o.default == "true")),
                Kind::Values(list) => {
                    let mut allowed = list.clone();
                    if !allowed.contains(&o.default) {
                        allowed.insert(0, o.default.clone());
                    }
                    let prefix = tr(format!("prefix.{name}")).unwrap_or_default();
                    let suffix = tr(format!("suffix.{name}")).unwrap_or_default();
                    hint.labels = allowed.iter().map(|v| (v.clone(), tr(format!("value.{name}.{v}")).unwrap_or_else(|| format!("{prefix}{v}{suffix}")))).collect();
                    hint.steps = sliders.contains(name.as_str());
                    hint.allowed = allowed;
                    Value::Scalar(Scalar::Str(o.default.clone()))
                }
            };
            let doc = tr(format!("option.{name}.comment")).unwrap_or_else(|| o.comment.clone());
            let label = tr(format!("option.{name}")).unwrap_or_else(|| readable(&name));
            Some(Field { section, key: name, label: Some(label), value, span: 0..0, item_spans: Vec::new(), doc, hint })
        })
        .collect();

    // Profiles: `profile.NAME = OPTION=value !SWITCH SWITCH profile.OTHER …`, offered first as one choice.
    // They may set options no screen shows: those are set and checked too, against every declared option.
    let defaults: HashMap<&str, &str> = opts.iter().map(|o| (o.name.as_str(), o.default.as_str())).collect();
    let raw: Vec<(&str, &str)> = props_list.iter().filter_map(|(k, v)| Some((k.strip_prefix("profile.")?, v.as_str()))).filter(|(n, _)| ident(n)).collect();
    fn resolve(name: &str, raw: &[(&str, &str)], depth: usize, out: &mut Vec<(String, String)>) {
        let Some((_, list)) = raw.iter().find(|(n, _)| *n == name).filter(|_| depth < MAX_DEPTH) else { return };
        for token in list.split_whitespace() {
            let (key, value) = if let Some(other) = token.strip_prefix("profile.") {
                resolve(other, raw, depth + 1, out);
                continue;
            } else if let Some((k, v)) = token.split_once('=') {
                (k, v)
            } else if let Some(k) = token.strip_prefix('!') {
                (k, "false")
            } else {
                (token, "true")
            };
            out.retain(|(k, _)| k != key);
            out.push((key.to_string(), value.to_string()));
        }
    }
    let presets: Vec<(String, Vec<(String, String, Option<String>)>)> = raw
        .iter()
        .map(|(name, _)| {
            let mut sets = Vec::new();
            resolve(name, &raw, 0, &mut sets);
            let sets = sets.into_iter().filter_map(|(k, v)| defaults.get(k.as_str()).map(|d| (k, v, Some(d.to_string())))).collect();
            (name.to_string(), sets)
        })
        .filter(|(_, sets): &(String, Vec<_>)| !sets.is_empty())
        .collect();
    if !presets.is_empty() {
        let mut labels: Vec<(String, String)> = presets.iter().map(|(n, _)| (n.clone(), tr(format!("profile.{n}")).unwrap_or_else(|| readable(n)))).collect();
        labels.push((CUSTOM.to_string(), "custom".to_string()));
        let hint = Hint { allowed: presets.iter().map(|(n, _)| n.clone()).collect(), labels, presets, on_off: true, ..Default::default() };
        let doc = tr("profile.comment".into()).unwrap_or_else(|| "A preset from the pack: sets many options at once. \"custom\" when they've been changed by hand.".into());
        let label = tr("profile".into()).unwrap_or_else(|| "profile".into());
        fields.insert(0, Field { section: Vec::new(), key: PROFILE.into(), label: Some(label), value: Value::Scalar(Scalar::Str(CUSTOM.into())), span: 0..0, item_spans: Vec::new(), doc, hint });
    }
    let mut d = Doc { fields, section_docs };
    d = doc(&d, "");
    d
}

/// The profile choice's key (never an option's name) and its value when no profile matches.
const PROFILE: &str = "<profile>";
const CUSTOM: &str = "<custom>";

/// Which profile the options match now, if any: shown options by their fields, ones no screen shows by
/// what `saved` holds for them, else their default.
fn current_profile(d: &Doc, saved: &HashMap<&str, &str>) -> String {
    let Some(p) = d.fields.iter().find(|f| f.key == PROFILE) else { return CUSTOM.into() };
    let now: HashMap<&str, String> = d.fields.iter().filter_map(|f| match &f.value {
        Value::Scalar(s) => Some((f.key.as_str(), s.display())),
        _ => None,
    }).collect();
    let value = |k: &str, default: &Option<String>| now.get(k).cloned().or_else(|| saved.get(k).map(|v| v.to_string())).or_else(|| default.clone());
    p.hint.presets.iter().find(|(_, sets)| sets.iter().all(|(k, v, d)| value(k, d).as_ref() == Some(v))).map_or_else(|| CUSTOM.into(), |(n, _)| n.clone())
}

/// `NAME=value` lines of a settings file.
fn saved(text: &str) -> HashMap<&str, &str> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).filter(|(k, _)| !k.starts_with('#')).collect()
}

/// The pack's options with the values saved in `text` laid over their defaults.
pub fn doc(base: &Doc, text: &str) -> Doc {
    let saved = saved(text);
    let mut d = base.clone();
    for f in &mut d.fields {
        let Some(v) = saved.get(f.key.as_str()).filter(|_| f.key != PROFILE) else { continue };
        f.value = match &f.value {
            Value::Scalar(Scalar::Bool(_)) => Value::Scalar(Scalar::Bool(*v == "true")),
            _ => Value::Scalar(Scalar::Str(v.to_string())),
        };
    }
    let profile = current_profile(&d, &saved);
    if let Some(p) = d.fields.iter_mut().find(|f| f.key == PROFILE) {
        p.value = Value::Scalar(Scalar::Str(profile));
    }
    d
}

/// `text` with one line set to `v` (or taken out when that's `default`).
fn set_line(text: &str, key: &str, v: &str, default: Option<&str>) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let at = lines.iter().position(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == key));
    match (at, default == Some(v)) {
        (Some(i), true) => {
            lines.remove(i);
        }
        (Some(i), false) => lines[i] = format!("{key}={v}"),
        (None, true) => {}
        (None, false) => lines.push(format!("{key}={v}")),
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `text` with `field` set to `new`: its line replaced or added, or taken out when it's back at the default
/// (as Iris writes them).
pub fn set(text: &str, field: &Field, new: &Value) -> anyhow::Result<String> {
    let Value::Scalar(s) = new else { bail!("{} takes a single value", field.key) };
    let v = s.display();
    // A profile sets each option it lists.
    if field.key == PROFILE {
        let Some((_, sets)) = field.hint.presets.iter().find(|(n, _)| *n == v) else { return Ok(text.to_string()) };
        return Ok(sets.iter().fold(text.to_string(), |t, (k, v, d)| set_line(&t, k, v, d.as_deref())));
    }
    Ok(set_line(text, &field.key, &v, field.hint.default.as_deref()))
}

/// A pack opened for editing: its settings file, with the pack's options as the base.
pub fn load(inst: &Instance, name: &str) -> anyhow::Result<Loaded> {
    let pack = dir(inst).join(checked(name)?);
    let files = read_pack(&pack).with_context(|| format!("couldn't read {name}"))?;
    let base = base(&files);
    if base.fields.is_empty() {
        bail!("{name} has no options to change");
    }
    let path = settings_path(inst, name)?;
    let text = if path.exists() { mc::read_text(&path)? } else { String::new() };
    Ok(Loaded { stamp: mc::Stamp::of(&path), format: Format::ShaderSettings, base: Some(Arc::new(base)), path, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(
            "lib/settings.glsl".into(),
            "#define SHADOWS // Shadows from the sun\n//#define BLOOM\n#define SHADOW_RES 2048 // [1024 2048 4096] Shadow map size\n#define WAVE_SPEED 1.0 // [0.5 1.0 1.5 2.0]\n#define INTERNAL 3\nconst float sunPathRotation = -40.0; // [-60.0 -40.0 0.0]\n".into(),
        );
        m.insert("composite.fsh".into(), "#ifdef SHADOWS\n#endif\n#if defined(BLOOM)\n#endif\n".into());
        m.insert("shaders.properties".into(), "# comment\nscreen = SHADOWS [LIGHT] <empty>\nscreen.LIGHT = SHADOW_RES BLOOM \\\n  WAVE_SPEED\nsliders = WAVE_SPEED\nprofile.LOW = !SHADOWS SHADOW_RES=1024\nprofile.HIGH = profile.LOW SHADOWS SHADOW_RES=4096 BLOOM\n".into());
        m.insert("lang/en_us.lang".into(), "option.SHADOWS=§aShadows\nscreen.LIGHT=Lighting\nvalue.SHADOW_RES.1024=Low\noption.WAVE_SPEED.comment=How fast water moves.\nsuffix.SHADOW_RES=px\n".into());
        m
    }

    #[test]
    fn options_come_from_the_packs_code_names_and_screens() {
        let d = base(&pack());
        let names: Vec<&str> = d.fields.iter().map(|f| f.key.as_str()).collect();
        // INTERNAL has no list and sunPathRotation isn't in a screen: neither is offered. Profiles come first.
        assert_eq!(names, [PROFILE, "SHADOWS", "SHADOW_RES", "BLOOM", "WAVE_SPEED"]);
        let f = |k: &str| d.fields.iter().find(|f| f.key == k).unwrap();
        assert_eq!(f("SHADOWS").label.as_deref(), Some("Shadows"), "formatting codes are dropped");
        assert_eq!(f("SHADOWS").section, Vec::<String>::new());
        assert_eq!(f("SHADOW_RES").section, ["Lighting"]);
        assert_eq!(f("BLOOM").value, Value::Scalar(Scalar::Bool(false)));
        assert_eq!(f("SHADOW_RES").hint.name_of("1024"), "Low");
        assert_eq!(f("SHADOW_RES").hint.name_of("4096"), "4096px");
        assert!(f("WAVE_SPEED").hint.steps);
        assert_eq!(f("WAVE_SPEED").doc, "How fast water moves.");
        assert_eq!(f("SHADOW_RES").doc, "Shadow map size");
    }

    #[test]
    fn settings_keep_only_what_differs_from_the_pack() {
        let b = base(&pack());
        let f = |d: &Doc, k: &str| d.fields.iter().find(|f| f.key == k).unwrap().clone();
        let t = set("", &f(&b, "SHADOW_RES"), &Value::Scalar(Scalar::Str("4096".into()))).unwrap();
        assert_eq!(t, "SHADOW_RES=4096\n");
        let t = set(&t, &f(&b, "BLOOM"), &Value::Scalar(Scalar::Bool(true))).unwrap();
        assert_eq!(t, "SHADOW_RES=4096\nBLOOM=true\n");
        let d = doc(&b, &t);
        assert_eq!(f(&d, "SHADOW_RES").value, Value::Scalar(Scalar::Str("4096".into())));
        assert_eq!(f(&d, "BLOOM").value, Value::Scalar(Scalar::Bool(true)));
        let t = set(&t, &f(&d, "SHADOW_RES"), &Value::Scalar(Scalar::Str("2048".into()))).unwrap();
        assert_eq!(t, "BLOOM=true\n", "back at the default, the line goes");
    }

    #[test]
    fn profiles_set_their_options_and_are_recognised() {
        let b = base(&pack());
        let f = |d: &Doc, k: &str| d.fields.iter().find(|f| f.key == k).unwrap().clone();
        assert_eq!(f(&b, PROFILE).value, Value::Scalar(Scalar::Str(CUSTOM.into())), "the defaults match no profile");
        let t = set("", &f(&b, PROFILE), &Value::Scalar(Scalar::Str("HIGH".into()))).unwrap();
        // HIGH builds on LOW and overrides it; options at their default aren't written.
        assert_eq!(t, "SHADOW_RES=4096\nBLOOM=true\n");
        let d = doc(&b, &t);
        assert_eq!(f(&d, PROFILE).value, Value::Scalar(Scalar::Str("HIGH".into())));
        let t = set(&t, &f(&d, PROFILE), &Value::Scalar(Scalar::Str("LOW".into()))).unwrap();
        // Like the game's menu, a profile sets only what it names: BLOOM stays on.
        assert_eq!(t, "SHADOW_RES=1024\nBLOOM=true\nSHADOWS=false\n");
        assert_eq!(f(&doc(&b, &t), PROFILE).value, Value::Scalar(Scalar::Str("LOW".into())), "LOW doesn't name BLOOM");
        let t = set(&t, &f(&doc(&b, &t), "SHADOWS"), &Value::Scalar(Scalar::Bool(true))).unwrap();
        assert_eq!(f(&doc(&b, &t), PROFILE).value, Value::Scalar(Scalar::Str(CUSTOM.into())));
    }

    /// A profile may set options no screen shows: choosing it writes them, and it's recognised only
    /// while they hold its values too.
    #[test]
    fn profiles_include_options_no_screen_shows() {
        let mut m = BTreeMap::new();
        m.insert("lib/settings.glsl".into(), "#define VISIBLE 0 // [0 1]\n#define HIDDEN 0 // [0 1 2]\n".into());
        m.insert("shaders.properties".into(), "screen = VISIBLE\nprofile.LOW = VISIBLE=1 HIDDEN=2\n".into());
        let b = base(&m);
        let f = |d: &Doc, k: &str| d.fields.iter().find(|f| f.key == k).unwrap().clone();
        assert!(!b.fields.iter().any(|f| f.key == "HIDDEN"), "still not shown");
        let t = set("", &f(&b, PROFILE), &Value::Scalar(Scalar::Str("LOW".into()))).unwrap();
        assert_eq!(t, "VISIBLE=1\nHIDDEN=2\n");
        assert_eq!(f(&doc(&b, &t), PROFILE).value, Value::Scalar(Scalar::Str("LOW".into())));
        // VISIBLE alone at LOW's value isn't LOW while HIDDEN is still at its default.
        assert_eq!(f(&doc(&b, "VISIBLE=1\n"), PROFILE).value, Value::Scalar(Scalar::Str(CUSTOM.into())));
    }

    /// A `const bool` with a comment is an option even though no `#ifdef` tests it (it's a GLSL
    /// value, not a macro); only valueless `#define` switches need testing.
    #[test]
    fn boolean_constants_are_options_without_an_ifdef() {
        let mut m = BTreeMap::new();
        m.insert("lib/settings.glsl".into(), "const bool shadowHardwareFiltering = true; // Filter shadows\n#define UNTESTED // a switch the code never tests\nconst bool plain = false;\n".into());
        m.insert("shadow.fsh".into(), "float x = shadowHardwareFiltering ? 1.0 : 0.0;\n".into());
        let b = base(&m);
        let names: Vec<&str> = b.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(names, ["shadowHardwareFiltering"]);
        let f = &b.fields[0];
        assert_eq!(f.value, Value::Scalar(Scalar::Bool(true)));
        assert_eq!(set("", f, &Value::Scalar(Scalar::Bool(false))).unwrap(), "shadowHardwareFiltering=false\n");
    }

    #[test]
    fn using_a_pack_changes_only_its_lines() {
        let mut inst = crate::instance::Instance::new("shader switch", "1.21.1", crate::loaders::Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        let g = inst.game_dir();
        std::fs::create_dir_all(g.join("shaderpacks/Pack ü.zip")).unwrap();
        std::fs::create_dir_all(g.join("config")).unwrap();
        assert_eq!(switch(&inst), None, "no shader mod, no file: nothing to write to");
        assert!(use_pack(&inst, Some("Pack ü.zip")).is_err());
        std::fs::write(g.join("config/iris.properties"), "#Iris\nmaxShadowRenderDistance=32\nenableShaders=false\nshaderPack=Other.zip\n").unwrap();
        use_pack(&inst, Some("Pack ü.zip")).unwrap();
        let t = std::fs::read_to_string(g.join("config/iris.properties")).unwrap();
        assert_eq!(t, "#Iris\nmaxShadowRenderDistance=32\nenableShaders=true\nshaderPack=Pack \\u00FC.zip\n", "escaped as Iris writes it");
        assert_eq!(active_pack(&inst).as_deref(), Some("Pack ü.zip"));
        use_pack(&inst, None).unwrap();
        assert_eq!(active_pack(&inst), None);
        assert!(use_pack(&inst, Some("../evil")).is_err());
        std::fs::remove_dir_all(inst.dir()).unwrap();
    }

    #[test]
    fn zipped_packs_are_read_with_or_without_a_top_folder() {
        let root = crate::paths::test_root().join("shader-zips");
        std::fs::create_dir_all(&root).unwrap();
        for (name, prefix) in [("flat.zip", ""), ("nested.zip", "Pack v1/")] {
            let path = root.join(name);
            let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            for (rel, text) in pack() {
                z.start_file(format!("{prefix}shaders/{rel}"), zip::write::SimpleFileOptions::default()).unwrap();
                std::io::Write::write_all(&mut z, text.as_bytes()).unwrap();
            }
            z.start_file(format!("{prefix}shaders/../evil.glsl"), zip::write::SimpleFileOptions::default()).unwrap();
            z.finish().unwrap();
            let files = read_pack(&path).unwrap();
            assert!(files.contains_key("shaders.properties") && files.contains_key("lib/settings.glsl"), "{name}");
            assert!(!files.keys().any(|k| k.contains("..")));
            assert_eq!(base(&files).fields.len(), 5);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
