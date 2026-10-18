//! The configs tab, in three parts: Minecraft's own settings (`options.txt`), the mods'
//! configs (grouped by the mod they belong to, TOML/JSON/JSON5 as fields, the rest as text)
//! and shader packs' options. Saving writes back only what changed (see `mod_config`), and
//! never over a file the game rewrote meanwhile without asking.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, CornerRadius, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui, vec2};

use super::instances::InstTab;
use super::nav::{Nav, Zone};
use super::widgets::{self as w, Lead};
use super::{App, Confirm, ConfirmAction, Modal, Tab, ToastKind};
use crate::instance::Instance;
use crate::keymap::Action;
use crate::mod_config::{self as mc, Doc, Field, Format, Group, ModInfo, Scalar, Value};
use crate::{fsutil, paths, shader_config};
use crate::task::Promise;
use crate::theme::{self, Theme};

/// How often the list of files is read again while the tab is open.
const RELIST: Duration = Duration::from_secs(5);
/// How often the open file is checked for changes on disk (the game rewrites configs).
const RECHECK: Duration = Duration::from_secs(2);
/// Lists longer than this start folded.
const FOLD_AT: usize = 6;
/// Text bigger than this is shown without colours (they're worked out every frame).
const HIGHLIGHT_MAX: usize = 200_000;

struct Listing {
    inst_id: String,
    groups: Vec<Group>,
    packs: Vec<shader_config::Pack>,
    /// Which shader mod picks the pack in use, if any.
    switch: Option<shader_config::Switch>,
    at: Instant,
}

#[derive(Default)]
pub struct ConfigState {
    listing: Option<Listing>,
    listing_p: Option<(String, Promise<(Vec<Group>, Vec<shader_config::Pack>, Option<shader_config::Switch>)>)>,
    /// Jars already read for their names, kept between listings.
    mods: Arc<Mutex<HashMap<String, ModInfo>>>,
    /// Relative to `config/`.
    selected: Option<PathBuf>,
    scroll_file: bool,
    file_filter: String,
    field_filter: String,
    /// The selected field, by [`field_id`].
    field_sel: Option<String>,
    scroll_field: bool,
    pub open: Option<Open>,
    loading: Option<(String, Area, PathBuf, Promise<mc::Loaded>)>,
    load_error: Option<(Area, PathBuf, String)>,
    /// Save or text/fields, routed here by the app (save also works while typing).
    pub key: Option<Action>,
    checked: Option<Instant>,
    /// The open file being checked on disk in the background.
    recheck: Option<Recheck>,
    /// Long lists the user unfolded, by [`field_id`].
    unfolded: HashSet<String>,
    /// File groups and settings headers that are open (all start folded), as left last time: see [`fold_key`].
    folds: Option<BTreeSet<String>>,
    /// Where the keyboard is in the file list: a group's heading or a file.
    cursor: Option<Item>,
    /// Which part of the tab is showing.
    pub area: Area,
    /// The shader packs, read with the file list.
    packs: Vec<shader_config::Pack>,
    /// A key binding waiting for its key (by [`field_id`]); the app sends key presses here meanwhile.
    pub capture: Option<String>,
    /// Modifiers held when the capture began, so a newly pressed shift, ctrl or alt can be told apart.
    capture_mods: Option<egui::Modifiers>,
    /// A captured binding, to apply: (field, `key.keyboard.…`).
    captured: Option<(String, String)>,
    /// When the last capture ended: the click that ended it mustn't start another.
    capture_ended: Option<Instant>,
}

/// The configs tab's three parts.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Area {
    #[default]
    Minecraft,
    Mods,
    Shaders,
}

impl Area {
    const ALL: [(Area, &'static str); 3] = [(Area::Minecraft, "minecraft"), (Area::Mods, "mods"), (Area::Shaders, "shaders")];

    fn key(self) -> &'static str {
        match self {
            Area::Minecraft => "minecraft",
            Area::Mods => "mods",
            Area::Shaders => "shaders",
        }
    }

    fn next(self) -> Area {
        match self {
            Area::Minecraft => Area::Mods,
            Area::Mods => Area::Shaders,
            Area::Shaders => Area::Minecraft,
        }
    }
}

/// Minecraft's own settings file, in the game folder.
const OPTIONS: &str = "options.txt";

/// A line of the file list.
#[derive(Clone, PartialEq)]
enum Item {
    Group(String),
    File(PathBuf),
}

/// Open file groups and settings headers, kept between runs.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Folds {
    #[serde(default)]
    open: BTreeSet<String>,
}

fn folds_path() -> PathBuf {
    paths::data_dir().join("config-view.toml")
}

/// The key a fold is remembered by: the instance, then the group's name or the file and its header.
fn fold_key(inst_id: &str, parts: &[&str]) -> String {
    std::iter::once(inst_id).chain(parts.iter().copied()).collect::<Vec<_>>().join("\u{1f}")
}

impl ConfigState {
    fn folds(&mut self) -> &mut BTreeSet<String> {
        self.folds.get_or_insert_with(|| std::fs::read_to_string(folds_path()).ok().and_then(|t| toml::from_str::<Folds>(&t).ok()).unwrap_or_default().open)
    }

    fn is_open(&mut self, key: &str) -> bool {
        self.folds().contains(key)
    }

    /// Open or fold `key`, and remember it.
    fn set_open(&mut self, key: String, open: bool) {
        let f = self.folds();
        let changed = if open { f.insert(key) } else { f.remove(&key) };
        if changed && let Ok(text) = toml::to_string(&Folds { open: f.clone() }) {
            let _ = fsutil::write_atomic(&folds_path(), text.as_bytes(), 0o644);
        }
    }

    /// Read `inst`'s config files in the background, now and then, while a tab needs them.
    fn refresh(&mut self, inst: &Instance, rt: &tokio::runtime::Handle, ctx: &egui::Context) {
        if self.listing.as_ref().is_some_and(|l| l.inst_id != inst.id) {
            self.listing = None;
            self.selected = None;
            self.cursor = None;
            self.file_filter.clear();
            self.field_filter.clear();
            self.field_sel = None;
        }
        if let Some((id, p)) = &self.listing_p
            && let Some(r) = p.take()
        {
            if *id == inst.id {
                let (groups, packs, switch) = r.unwrap_or_default();
                self.packs = packs.clone();
                self.listing = Some(Listing { inst_id: id.clone(), groups, packs, switch, at: Instant::now() });
                // A file that couldn't be opened gets another try with each fresh listing.
                self.load_error = None;
            }
            self.listing_p = None;
        }
        let stale = self.listing.as_ref().is_none_or(|l| l.at.elapsed() > RELIST);
        if stale && self.listing_p.as_ref().is_none_or(|(id, _)| *id != inst.id) {
            let (i, cache) = (inst.clone(), self.mods.clone());
            let p = Promise::spawn(rt, ctx, async move {
                Ok(tokio::task::spawn_blocking(move || {
                    let files = mc::list(&i);
                    let mods = mc::known_mods(&i, &mut cache.lock().unwrap());
                    (mc::group(&files, &mods), shader_config::packs(&i), shader_config::switch(&i))
                })
                .await?)
            });
            self.listing_p = Some((inst.id.clone(), p));
            ctx.request_repaint_after(RELIST);
        }
    }

    /// `inst`'s shader packs' names, once listed.
    pub fn pack_names(&mut self, inst: &Instance, rt: &tokio::runtime::Handle, ctx: &egui::Context) -> Vec<String> {
        self.refresh(inst, rt, ctx);
        self.listing.as_ref().map(|l| l.packs.iter().map(|p| p.name.clone()).collect()).unwrap_or_default()
    }

    /// `inst`'s mods' config groups, as (the mod ids each answers to, its name), once its files are listed.
    pub fn mod_groups(&mut self, inst: &Instance, rt: &tokio::runtime::Handle, ctx: &egui::Context) -> Vec<(Vec<String>, String)> {
        self.refresh(inst, rt, ctx);
        let Some(l) = &self.listing else { return Vec::new() };
        l.groups.iter().filter(|g| g.name != mc::OTHER).map(|g| (g.ids.clone(), g.name.clone())).collect()
    }
}


/// Show shader pack `name`'s options.
pub fn open_pack(app: &mut App, name: &str) {
    app.inst_tab = InstTab::Config;
    app.focus = Zone::Main;
    app.config.cursor = Some(Item::File(PathBuf::from(name)));
    app.config.scroll_file = true;
    pick(app, Area::Shaders, PathBuf::from(name));
}

/// Show the config files of `inst`'s mod group `name`: the group open and its first file selected.
pub fn open_group(app: &mut App, inst: &Instance, name: &str) {
    let Some(first) = app.config.listing.as_ref().and_then(|l| l.groups.iter().find(|g| g.name == name)).and_then(|g| g.files.first()).map(|f| f.rel.clone()) else { return };
    app.config.set_open(fold_key(&inst.id, &["group", name]), true);
    app.inst_tab = InstTab::Config;
    app.config.area = Area::Mods;
    app.focus = Zone::Main;
    app.config.cursor = Some(Item::File(first.clone()));
    app.config.scroll_file = true;
    pick(app, Area::Mods, first);
}

/// The file being edited.
pub struct Open {
    pub inst_id: String,
    pub area: Area,
    /// Relative to `config/` for mods; the pack's name for shaders; `options.txt` for Minecraft.
    pub rel: PathBuf,
    path: PathBuf,
    format: Format,
    /// The text as it is on disk (as last loaded or saved).
    saved: String,
    text: String,
    stamp: Option<mc::Stamp>,
    doc: Result<Doc, mc::ParseError>,
    /// Each field's value on disk, to mark the ones changed here.
    saved_values: HashMap<String, Value>,
    raw: bool,
    /// The file changed on disk while there were unsaved changes here: its text now.
    conflict: Option<String>,
    /// A quiet word about what just happened (reloaded, saved).
    note: Option<(String, Instant)>,
    /// A shader pack's options, which its settings file's values are laid over.
    base: Option<Arc<Doc>>,
    /// Changes whenever `saved` or `stamp` does (loaded, reloaded, saved), so a check of the file
    /// made before that is dropped rather than applied over it.
    generation: u64,
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Which open file a check was made of: instance, area, file and [`Open::generation`].
#[derive(Debug, Clone, PartialEq)]
struct CheckOf {
    inst_id: String,
    area: Area,
    rel: PathBuf,
    generation: u64,
}

/// A check of the open file on disk, run off the UI thread (the file may be big, or on slow storage).
struct Recheck {
    of: CheckOf,
    promise: Promise<Reread>,
}

/// What a check found: the file's stamp now and, if it changed, its text and fields.
struct Reread {
    stamp: Option<mc::Stamp>,
    changed: Option<(String, Result<Doc, mc::ParseError>)>,
}

/// Read and parse `path` again if its stamp isn't `was` any more. Blocking: run it on a Promise.
fn reread(path: &std::path::Path, was: Option<mc::Stamp>, format: Format, base: Option<&Doc>) -> Reread {
    let stamp = mc::Stamp::of(path);
    if stamp == was {
        return Reread { stamp, changed: None };
    }
    let disk = if path.exists() { mc::read_text(path).ok() } else { Some(String::new()) };
    let changed = disk.map(|d| {
        let doc = mc::parse_with(format, base, &d);
        (d, doc)
    });
    Reread { stamp, changed }
}

fn field_id(f: &Field) -> String {
    format!("{}\u{1f}{}", f.section.join("\u{1f}"), f.key)
}

fn values(doc: &Result<Doc, mc::ParseError>) -> HashMap<String, Value> {
    doc.as_ref().map(|d| d.fields.iter().map(|f| (field_id(f), f.value.clone())).collect()).unwrap_or_default()
}

impl Open {
    fn new(inst_id: String, area: Area, rel: PathBuf, loaded: mc::Loaded) -> Self {
        let format = loaded.format;
        let doc = mc::parse_with(format, loaded.base.as_deref(), &loaded.text);
        // Fields where there are some to show; text otherwise.
        let raw = !format.structured() || doc.as_ref().map_or(true, |d| d.fields.is_empty());
        Open {
            inst_id,
            area,
            rel,
            path: loaded.path,
            format,
            saved_values: values(&doc),
            saved: loaded.text.clone(),
            text: loaded.text,
            stamp: loaded.stamp,
            doc,
            raw,
            conflict: None,
            note: None,
            base: loaded.base,
            generation: next_generation(),
        }
    }

    fn check_of(&self) -> CheckOf {
        CheckOf { inst_id: self.inst_id.clone(), area: self.area, rel: self.rel.clone(), generation: self.generation }
    }

    /// Take what a background check found, if it was made of this very file as it is here now;
    /// false (and nothing changed) if it wasn't.
    fn take_reread(&mut self, of: &CheckOf, r: Reread) -> bool {
        if *of != self.check_of() {
            return false;
        }
        self.stamp = r.stamp;
        if let Some((disk, doc)) = r.changed
            && disk != self.saved
        {
            if self.dirty() {
                self.conflict = Some(disk);
            } else {
                self.saved = disk.clone();
                self.text = disk;
                self.doc = doc;
                self.saved_values = values(&self.doc);
                self.conflict = None;
                self.generation = next_generation();
                self.note = Some(("reloaded: the file changed on disk".into(), Instant::now()));
            }
        }
        true
    }

    pub fn dirty(&self) -> bool {
        self.text != self.saved
    }

    /// Shown as text rather than fields.
    pub fn raw_mode(&self) -> bool {
        self.raw || !self.format.structured()
    }

    /// Whether it can be shown as fields at all.
    pub fn has_fields(&self) -> bool {
        self.format.structured()
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
        self.doc = mc::parse_with(self.format, self.base.as_deref(), &self.text);
    }

    /// Take the file as it is on disk now.
    fn reload(&mut self, text: String) {
        self.saved = text.clone();
        self.set_text(text);
        self.saved_values = values(&self.doc);
        self.stamp = mc::Stamp::of(&self.path);
        self.conflict = None;
        self.generation = next_generation();
    }

    fn name(&self) -> String {
        self.rel.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }
}

/// Somewhere to go once unsaved changes are dealt with.
pub enum Away {
    File(Area, PathBuf),
    View { tab: Tab, inst_tab: InstTab, selected: Option<String> },
    Quit,
}

/// "Unsaved changes" before leaving a file.
pub struct Leave {
    away: Away,
}

fn glyph(format: Format) -> &'static str {
    match format {
        Format::Toml => "\u{e6b2}",
        Format::Json | Format::Json5 => "\u{e60b}",
        Format::Text => "\u{f0f6}",
        Format::Options => "\u{f013}",
        Format::ShaderSettings => "\u{f05a8}",
    }
}

/// Files in list order, as filtered.
fn visible(groups: &[Group], filter: &str) -> Vec<(usize, Vec<PathBuf>)> {
    let f = filter.trim().to_lowercase();
    groups
        .iter()
        .enumerate()
        .map(|(n, g)| {
            let all = f.is_empty() || g.name.to_lowercase().contains(&f);
            (n, g.files.iter().filter(|c| all || c.rel.to_string_lossy().to_lowercase().contains(&f)).map(|c| c.rel.clone()).collect::<Vec<_>>())
        })
        .filter(|(_, files)| !files.is_empty())
        .collect()
}

fn shown_fields<'a>(doc: &'a Doc, filter: &str) -> Vec<(usize, &'a Field)> {
    let f = filter.trim().to_lowercase();
    doc.fields
        .iter()
        .enumerate()
        .filter(|(_, x)| f.is_empty() || x.key.to_lowercase().contains(&f) || x.doc.to_lowercase().contains(&f) || x.section.iter().any(|s| s.to_lowercase().contains(&f)))
        .collect()
}

/// Ask before leaving a file with unsaved changes; otherwise go straight there.
fn pick_file(app: &mut App, rel: PathBuf) {
    pick(app, app.config.area, rel);
}

/// Show `rel` in `area`, asking first if the open file has unsaved changes.
fn pick(app: &mut App, area: Area, rel: PathBuf) {
    let s = &mut app.config;
    if s.open.as_ref().is_some_and(|o| o.dirty() && (o.rel != rel || o.area != area)) {
        app.modal = Some(Modal::ConfigLeave(Box::new(Leave { away: Away::File(area, rel) })));
        return;
    }
    if s.area != area {
        s.area = area;
        s.selected = None;
    }
    if s.selected.as_ref() != Some(&rel) {
        s.selected = Some(rel);
        s.field_sel = None;
        s.field_filter.clear();
    }
}

/// What a frame of the tab asked for, done once drawing is over.
#[derive(Default)]
struct Out {
    pick: Option<PathBuf>,
    /// Open or fold this group or header ([`fold_key`]).
    fold: Option<String>,
    /// Wait for a key for this binding.
    capture: Option<String>,
    /// Put every setting with a known default back to it.
    defaults: bool,
    /// Switch shaders to this pack, or off.
    use_pack: Option<Option<String>>,
    scroll_file: bool,
    edit: Option<(usize, Value)>,
    save: bool,
    revert: bool,
    raw: Option<bool>,
    take_disk: bool,
    keep_mine: bool,
    focus: Option<Zone>,
}

/// Read `rel` of `area` for editing.
fn load_for(area: Area, inst: &Instance, rel: &std::path::Path) -> anyhow::Result<mc::Loaded> {
    match area {
        Area::Mods => mc::load(inst, rel),
        Area::Shaders => shader_config::load(inst, &rel.to_string_lossy()),
        Area::Minecraft => {
            let path = inst.game_dir().join(OPTIONS);
            let text = mc::read_text(&path)?;
            Ok(mc::Loaded { stamp: mc::Stamp::of(&path), format: Format::Options, base: None, path, text })
        }
    }
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let running = app.is_running(&inst.id);
    let list_nav = app.nav_for(Zone::Main);
    let edit_nav = app.nav_for(Zone::Side);
    let focus = app.focus;
    let key = app.config.key.take();
    let rt = app.rt.clone();
    let icons = if app.config.area == Area::Shaders { app.content.icons(inst, crate::instance::ContentKind::ShaderPacks, &rt, &ctx) } else { HashMap::new() };
    let s = &mut app.config;

    // Another instance's file with nothing unsaved: let it go (unsaved ones wait in `guard`).
    if s.open.as_ref().is_some_and(|o| o.inst_id != inst.id && !o.dirty()) {
        s.open = None;
    }
    s.refresh(inst, &rt, &ctx);
    s.folds();
    let mut out = Out::default();

    // The three parts. Switching shows the file left open there, if any.
    let mut area = s.area;
    ui.horizontal(|ui| {
        w::segmented(ui, &t, &mut area, &Area::ALL);
        ui.add_space(12.0);
        let what = match area {
            Area::Minecraft => "the game's own settings: options.txt",
            Area::Mods => "each mod's files in config/",
            Area::Shaders => "each shader pack's options",
        };
        ui.add(egui::Label::new(RichText::new(what).color(t.muted)).truncate());
    });
    ui.add_space(10.0);
    if key == Some(Action::ConfigArea) {
        area = s.area.next();
    }
    if area != s.area {
        s.area = area;
        s.selected = s.open.as_ref().filter(|o| o.area == area && o.inst_id == inst.id).map(|o| o.rel.clone());
        s.field_sel = None;
        s.field_filter.clear();
        s.cursor = None;
    }
    let Some(listing) = &s.listing else {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("looking for config files…").color(t.muted));
        });
        return;
    };
    let groups = listing.groups.clone();
    let packs = listing.packs.clone();
    let switch = listing.switch;

    // What to list on the left, and what the editor's keys are.
    let mut edit_nav = edit_nav;
    let mut editor_focused = focus == Zone::Side;
    let mut items: Vec<Item> = Vec::new();
    match s.area {
        Area::Minecraft => {
            if !inst.game_dir().join(OPTIONS).is_file() {
                w::empty_state(ui, &t, "\u{f013}", "no options yet", "Minecraft writes options.txt the first time it starts. Play once, then come back here.");
                return;
            }
            s.selected = Some(PathBuf::from(OPTIONS));
            // One file, no list: the editor has the keyboard.
            edit_nav = list_nav.or(edit_nav);
            editor_focused = matches!(focus, Zone::Main | Zone::Side);
        }
        Area::Mods => {
            if groups.is_empty() {
                w::empty_state(ui, &t, "\u{f107b}", "no config files yet", "Mods write their settings into config/ the first time the game starts with them. Play once, then come back here.");
                return;
            }
            let shown = visible(&groups, &s.file_filter);
            let all: Vec<&PathBuf> = groups.iter().flat_map(|g| g.files.iter().map(|f| &f.rel)).collect();
            // A file that's gone (and not being edited) drops its selection.
            if s.selected.as_ref().is_some_and(|sel| !all.contains(&sel) && s.open.as_ref().is_none_or(|o| o.rel != *sel || !o.dirty())) {
                s.selected = None;
            }
            // A mod with one file is that file; others are a heading, and their files when open (always while
            // filtering, so what matches shows).
            let filtering = !s.file_filter.trim().is_empty();
            for (n, files) in &shown {
                let g = &groups[*n];
                if g.files.len() == 1 {
                    items.push(Item::File(g.files[0].rel.clone()));
                    continue;
                }
                items.push(Item::Group(g.name.clone()));
                if filtering || s.is_open(&fold_key(&inst.id, &["group", &g.name])) {
                    items.extend(files.iter().cloned().map(Item::File));
                }
            }
        }
        Area::Shaders => {
            if packs.is_empty() {
                w::empty_state(ui, &t, "\u{f0eb}", "no shader packs", "Add packs in the shader packs tab; their options show up here.");
                return;
            }
            if s.selected.as_ref().is_some_and(|sel| !packs.iter().any(|p| *sel == PathBuf::from(&p.name))) {
                s.selected = None;
            }
            items = packs.iter().map(|p| Item::File(PathBuf::from(&p.name))).collect();
        }
    }
    if s.cursor.as_ref().is_none_or(|c| !items.contains(c)) {
        s.cursor = s.selected.clone().map(Item::File).filter(|c| items.contains(c)).or_else(|| items.first().cloned());
    }

    // Keyboard on the list: moving onto a file opens it; enter or space on a heading folds it.
    if s.area != Area::Minecraft
        && let Some(n) = list_nav
    {
        let pos = s.cursor.as_ref().and_then(|c| items.iter().position(|i| i == c));
        let here = pos.and_then(|p| items.get(p)).cloned();
        match (n, here) {
            (n, _) if n.is_move() => {
                if let Some(p) = n.step(items.len(), pos) {
                    s.cursor = Some(items[p].clone());
                    if let Item::File(rel) = &items[p] {
                        out.pick = Some(rel.clone());
                    }
                    out.scroll_file = true;
                }
            }
            (Nav::Activate | Nav::Toggle, Some(Item::Group(name))) => out.fold = Some(fold_key(&inst.id, &["group", &name])),
            (Nav::Activate | Nav::Edit, Some(Item::File(_))) => out.focus = Some(Zone::Side),
            (Nav::Folder, _) => super::open_path(&if s.area == Area::Shaders { shader_config::dir(inst) } else { mc::config_dir(inst) }),
            _ => {}
        }
    }

    // Load the selected file (in the background) unless it's the one open.
    if let Some((id, area, rel, p)) = &s.loading
        && let Some(r) = p.take()
    {
        let (id, area, rel) = (id.clone(), *area, rel.clone());
        s.loading = None;
        if id == inst.id && area == s.area && s.selected.as_ref() == Some(&rel) {
            match r {
                Ok(loaded) => {
                    s.open = Some(Open::new(id, area, rel, loaded));
                    s.load_error = None;
                }
                Err(e) => s.load_error = Some((area, rel, format!("{e:#}"))),
            }
        }
    }
    let area = s.area;
    if let Some(sel) = s.selected.clone() {
        let have = s.open.as_ref().is_some_and(|o| o.rel == sel && o.area == area && o.inst_id == inst.id);
        let pending = s.loading.as_ref().is_some_and(|(id, a, r, _)| *r == sel && *a == area && *id == inst.id);
        let failed = s.load_error.as_ref().is_some_and(|(a, r, _)| *r == sel && *a == area);
        // An unsaved file elsewhere stays open (leaving the tab asks about it); anything else is replaced.
        let blocked = s.open.as_ref().is_some_and(|o| o.dirty() && !(o.rel == sel && o.area == area));
        if !have && !pending && !failed && !blocked {
            let (i, r) = (inst.clone(), sel.clone());
            let p = Promise::spawn(&rt, &ctx, async move { tokio::task::spawn_blocking(move || load_for(area, &i, &r)).await? });
            s.loading = Some((inst.id.clone(), area, sel, p));
        }
    }

    // The game (or anything else) may rewrite the open file. It's checked, read and parsed on a
    // Promise; the file stays as shown until the result comes, and a result for a file that has
    // since been reloaded, saved or closed is dropped.
    if let Some(c) = &s.recheck
        && let Some(r) = c.promise.take()
    {
        let c = s.recheck.take().unwrap();
        if let (Ok(r), Some(o)) = (r, &mut s.open) {
            o.take_reread(&c.of, r);
        }
    }
    if s.recheck.is_none()
        && let Some(o) = &s.open
        && s.checked.is_none_or(|c| c.elapsed() > RECHECK)
    {
        s.checked = Some(Instant::now());
        ctx.request_repaint_after(RECHECK);
        let (path, was, format, base) = (o.path.clone(), o.stamp, o.format, o.base.clone());
        let promise = Promise::spawn(&rt, &ctx, async move { Ok(tokio::task::spawn_blocking(move || reread(&path, was, format, base.as_deref())).await?) });
        s.recheck = Some(Recheck { of: o.check_of(), promise });
    }

    // A binding's captured key.
    if let Some((id, v)) = s.captured.take()
        && let Some(o) = &s.open
        && let Ok(doc) = &o.doc
        && let Some(n) = doc.fields.iter().position(|f| field_id(f) == id)
    {
        out.edit = Some((n, Value::Scalar(Scalar::Str(v))));
    }

    // Keys routed from the app.
    match key {
        Some(Action::SaveConfig) => out.save = true,
        Some(Action::RawConfig) => out.raw = s.open.as_ref().filter(|o| o.format.structured()).map(|o| !o.raw),
        _ => {}
    }

    if area != Area::Minecraft {
        // Narrow windows give the list less, so the editor keeps room to work.
        let avail = ui.available_width();
        let pane_w = if avail < 720.0 { (avail * 0.36).max(150.0) } else { (avail * 0.3).clamp(220.0, 300.0) };
        egui::Panel::left("config-files").exact_size(pane_w).resizable(false).frame(egui::Frame::new().inner_margin(egui::Margin { right: 14, ..Default::default() })).show(ui, |ui| {
            if area == Area::Shaders {
                packs_pane(ui, &t, s, &packs, &icons, focus == Zone::Main, &mut out);
            } else {
                files_pane(ui, &t, s, &inst.id, &groups, &items, list_nav == Some(Nav::Search), focus == Zone::Main, &mut out);
            }
        });
    }

    // The pack in use, switched here as the game's shader menu would (Iris's or OptiFine's own file).
    let pack = (area == Area::Shaders).then(|| {
        let sel = s.selected.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let why = match (switch, running) {
            (None, _) => Some("no shader mod (Iris or OptiFine) in this instance"),
            (_, true) => Some("close the game first: it saves its own choice when it quits"),
            _ => None,
        };
        PackSwitch { active: packs.iter().any(|p| p.name == sel && p.active), why, optifine: switch == Some(shader_config::Switch::OptiFine), name: sel }
    });
    match (&mut s.open, &s.load_error) {
        (Some(o), _) if s.selected.as_ref() == Some(&o.rel) && o.area == area && o.inst_id == inst.id => {
            let folds = s.folds.get_or_insert_with(Default::default);
            let cooling = s.capture_ended.is_some_and(|e| e.elapsed() < Duration::from_millis(400));
            let st = EditorState { pack: pack.as_ref(), folds, field_filter: &mut s.field_filter, field_sel: &mut s.field_sel, scroll_field: &mut s.scroll_field, unfolded: &mut s.unfolded, capture: s.capture.as_deref(), cooling };
            editor(ui, &t, o, st, running, edit_nav, editor_focused, &mut out);
        }
        (_, Some((a, rel, e))) if *a == area && s.selected.as_ref() == Some(rel) => {
            let what = if area == Area::Shaders { "can't read this pack's options" } else { "can't open this file" };
            w::empty_state(ui, &t, "\u{f05d6}", what, e);
        }
        (Some(o), _) if o.dirty() && s.selected.is_some() => {
            let name = o.rel.display().to_string();
            w::empty_state(ui, &t, "\u{f0193}", "unsaved changes elsewhere", &format!("{name} has unsaved changes. Save or discard them there first."));
        }
        _ if s.selected.is_none() => match area {
            Area::Shaders => w::empty_state(ui, &t, "\u{f0eb}", "pick a shader pack", "Its options show here, named and grouped as in the game's shader menu."),
            _ => w::empty_state(ui, &t, "\u{f107b}", "pick a config file", "Mods are listed on the left. Open one to see its config files, then pick a file to edit its settings."),
        },
        _ => {
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                ui.spinner();
                if area == Area::Shaders {
                    ui.label(RichText::new("reading the pack's options…").color(t.muted));
                }
            });
        }
    }

    apply(app, out);
}

/// A shader pack: its icon (as the shader packs tab shows it), its name on up to three lines with "in use" under it
/// for the pack switched on, and a note on the right.
fn pack_row(ui: &mut Ui, t: &Theme, selected: bool, active: bool, icon: Option<&std::path::Path>, name: &str, right: &str) -> egui::Response {
    let size = t.body_size();
    let width = ui.available_width();
    let p = ui.painter();
    let note = (!right.is_empty()).then(|| p.layout_no_wrap(right.to_string(), FontId::monospace(size - 1.0), t.accent));
    let note_w = note.as_ref().map_or(0.0, |g| g.size().x + 12.0);
    let icon_size = size * 2.0;
    let text_x = 10.0 + icon_size + 10.0;
    let mut job = LayoutJob::single_section(name.to_string(), TextFormat::simple(FontId::monospace(size), if selected { t.menu_selected_text } else { t.fg }));
    job.wrap = egui::text::TextWrapping { max_width: (width - text_x - note_w - 10.0).max(40.0), max_rows: 3, break_anywhere: false, overflow_character: Some('…') };
    let g = p.layout_job(job);
    let tag = active.then(|| p.layout_no_wrap("● in use".to_string(), FontId::monospace(size - 2.0), t.green));
    let tag_h = tag.as_ref().map_or(0.0, |g| g.size().y + 2.0);
    let h = (g.size().y + tag_h + 16.0).max(icon_size + 14.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected {
            p.rect(rect, CornerRadius::ZERO, t.fg_a(t.menu_selected_bg_alpha), Stroke::new(1.0, t.fg_a(t.menu_selected_border_alpha)), StrokeKind::Inside);
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::ZERO, t.fg_a(t.hover_fill_alpha));
        }
        let ir = Rect::from_center_size(egui::pos2(rect.left() + 10.0 + icon_size / 2.0, rect.center().y), vec2(icon_size, icon_size));
        match icon {
            Some(path) => {
                egui::Image::new(egui::ImageSource::Uri(format!("file://{}", path.display()).into())).corner_radius(3.0).paint_at(ui, ir);
            }
            // Unzipped packs and packs Modrinth doesn't know: the shader packs tab's own glyph.
            None => w::glyph_centered(p, ir.center(), "\u{f05a8}", FontId::monospace(size + 2.0), t.muted),
        }
        let top = rect.center().y - (g.size().y + tag_h) / 2.0;
        let gh = g.size().y;
        p.galley(egui::pos2(rect.left() + text_x, top), g, t.fg);
        if let Some(tag) = tag {
            p.galley(egui::pos2(rect.left() + text_x, top + gh + 2.0), tag, t.green);
        }
        if let Some(n) = note {
            let (nw, nh) = (n.size().x, n.size().y);
            p.galley(egui::pos2(rect.right() - 10.0 - nw, rect.center().y - nh / 2.0), n, t.accent);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The shader packs, the one in use marked.
fn packs_pane(ui: &mut Ui, t: &Theme, s: &mut ConfigState, packs: &[shader_config::Pack], icons: &HashMap<String, PathBuf>, focused: bool, out: &mut Out) {
    let r = ui.max_rect();
    ui.painter().vline(r.right() + 7.0, r.y_range(), Stroke::new(1.0, t.fg_a(0.12)));
    let open_dirty = s.open.as_ref().filter(|o| o.dirty() && o.area == Area::Shaders).map(|o| o.rel.clone());
    let scroll = std::mem::take(&mut s.scroll_file);
    egui::ScrollArea::vertical().id_salt("shader-packs").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for p in packs {
            let rel = PathBuf::from(&p.name);
            let item = Item::File(rel.clone());
            let on = s.selected.as_ref() == Some(&rel);
            let at = s.cursor.as_ref() == Some(&item);
            // Pack names are long and alike ("ComplementaryReimagined_r5.8.1 + EuphoriaPatches_1.9.3"): up to two
            // lines, broken between words.
            let shown = p.name.strip_suffix(".zip").or_else(|| p.name.strip_suffix(".ZIP")).unwrap_or(&p.name).replace('_', " ");
            let dot = if open_dirty.as_ref() == Some(&rel) { "●" } else { "" };
            let hover = if p.active { format!("{}\nin use", p.name) } else { p.name.clone() };
            let r = pack_row(ui, t, on, p.active, icons.get(&p.name).map(|p| p.as_path()), &shown, dot).on_hover_text(hover);
            if at && focused {
                w::focus_mark(ui, t, r.rect);
            }
            if at && scroll {
                r.scroll_to_me(None);
            }
            if r.clicked() {
                s.cursor = Some(item);
                out.pick = Some(rel);
                out.focus = Some(Zone::Main);
            }
            if r.double_clicked() {
                out.focus = Some(Zone::Side);
            }
        }
    });
}

/// Carry out what the frame asked for.
fn apply(app: &mut App, out: Out) {
    if let Some(z) = out.focus {
        app.focus = z;
    }
    if let Some(k) = out.fold {
        let open = !app.config.is_open(&k);
        app.config.set_open(k, open);
    }
    if let Some(pack) = out.use_pack
        && let Some(inst) = app.selected_instance().cloned()
    {
        match shader_config::use_pack(&inst, pack.as_deref()) {
            Ok(()) => {
                // Shown at once; the next listing reads it back from the file.
                for l in app.config.listing.iter_mut().flat_map(|l| l.packs.iter_mut()) {
                    l.active = pack.as_deref() == Some(l.name.as_str());
                }
                let what = match &pack {
                    Some(p) => format!("Shaders now use {}", p.trim_end_matches(".zip")),
                    None => "Shaders turned off".to_string(),
                };
                app.toast(ToastKind::Info, what);
            }
            Err(e) => app.toast(ToastKind::Error, format!("{e:#}")),
        }
    }
    if let Some(id) = out.capture {
        app.config.capture = Some(id);
        app.config.capture_mods = None;
    }
    if let Some(rel) = out.pick {
        pick_file(app, rel);
        app.config.scroll_file = out.scroll_file;
    }
    let s = &mut app.config;
    if let Some(o) = &mut s.open {
        if let Some((n, v)) = out.edit
            && let Ok(doc) = &o.doc
            && let Some(f) = doc.fields.get(n)
        {
            match mc::set(o.format, &o.text, f, &v) {
                Ok(text) => o.set_text(text),
                Err(e) => app.toasts.push(super::Toast::error(format!("{e:#}"))),
            }
        }
        if out.defaults {
            // One at a time: each edit moves the others' places in the text.
            for _ in 0..=o.doc.as_ref().map_or(0, |d| d.fields.len()) {
                let Ok(doc) = &o.doc else { break };
                let Some((f, d)) = doc.fields.iter().find_map(|f| default_value(f).filter(|d| Value::Scalar(d.clone()) != f.value).map(|d| (f.clone(), d))) else { break };
                match mc::set(o.format, &o.text, &f, &Value::Scalar(d)) {
                    Ok(text) => o.set_text(text),
                    Err(e) => {
                        app.toasts.push(super::Toast::error(format!("{e:#}")));
                        break;
                    }
                }
            }
            o.note = Some(("defaults restored; save to keep them".into(), Instant::now()));
        }
        if let Some(raw) = out.raw {
            if !raw && o.doc.is_err() {
                app.toasts.push(super::Toast::error("Fix the error in the text first; then it opens as fields again"));
            } else {
                o.raw = raw;
            }
        }
        if out.take_disk
            && let Some(disk) = o.conflict.take()
        {
            o.reload(disk);
            o.note = Some(("took the file as it is on disk".into(), Instant::now()));
        }
        if out.keep_mine
            && let Some(disk) = o.conflict.take()
        {
            // What's on disk now is what saving will replace.
            o.saved = disk;
            o.saved_values = values(&mc::parse_with(o.format, o.base.as_deref(), &o.saved));
            o.stamp = mc::Stamp::of(&o.path);
            o.generation = next_generation();
        }
    }
    if out.revert && app.config.open.as_ref().is_some_and(|o| o.dirty()) {
        let name = app.config.open.as_ref().map(|o| o.name()).unwrap_or_default();
        app.modal = Some(Modal::Confirm(Confirm {
            title: "discard changes".into(),
            body: format!("Put {name} back as it is on disk? The changes made here are lost."),
            button: "discard".into(),
            action: ConfirmAction::RevertConfig,
        }));
    }
    if out.save {
        save(app, false);
    }
}

/// Put the open file back as it is on disk.
pub fn revert(app: &mut App) {
    if let Some(o) = &mut app.config.open {
        let saved = o.saved.clone();
        o.set_text(saved);
        o.conflict = None;
        o.note = Some(("changes discarded".into(), Instant::now()));
    }
}

/// Save the open file; false if it couldn't be (an error, or it changed on disk).
fn save(app: &mut App, force: bool) -> bool {
    let Some(o) = &mut app.config.open else { return false };
    if !o.dirty() && !force {
        return true;
    }
    match mc::save(&o.path, &o.saved, &o.text, force) {
        Ok(mc::Saved::Done(stamp)) => {
            o.saved = o.text.clone();
            o.saved_values = values(&o.doc);
            o.stamp = stamp;
            o.conflict = None;
            o.generation = next_generation();
            // Text that doesn't parse may still be saved: it's the user's file. Say so.
            let note = if o.format.structured() && o.doc.is_err() { "saved, with an error in it: the mod may not read it" } else { "saved" };
            o.note = Some((note.into(), Instant::now()));
            true
        }
        Ok(mc::Saved::Changed(now)) => {
            o.conflict = Some(now);
            false
        }
        Err(e) => {
            let name = o.name();
            app.toast(ToastKind::Error, format!("Couldn't save {name}: {e:#}"));
            false
        }
    }
}

// -- the file list ----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn files_pane(ui: &mut Ui, t: &Theme, s: &mut ConfigState, inst_id: &str, groups: &[Group], items: &[Item], focus_filter: bool, focused: bool, out: &mut Out) {
    let r = ui.max_rect();
    ui.painter().vline(r.right() + 7.0, r.y_range(), Stroke::new(1.0, t.fg_a(0.12)));
    let resp = w::text_input(ui, &mut s.file_filter, "/ filter files or mods", ui.available_width(), focus_filter);
    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        s.file_filter.clear();
        resp.surrender_focus();
    }
    ui.add_space(6.0);
    let open_dirty = s.open.as_ref().filter(|o| o.dirty()).map(|o| o.rel.clone());
    let scroll = std::mem::take(&mut s.scroll_file);
    let filtering = !s.file_filter.trim().is_empty();
    egui::ScrollArea::vertical().id_salt("config-files").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        if items.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("nothing matches").color(t.muted));
        }
        for item in items {
            let at = s.cursor.as_ref() == Some(item);
            let r = match item {
                Item::Group(name) => {
                    let Some(g) = groups.iter().find(|g| g.name == *name) else { continue };
                    let open = filtering || s.is_open(&fold_key(inst_id, &["group", name]));
                    let r = group_header(ui, t, g, open, at).on_hover_text(format!("{} config files", g.files.len()));
                    if r.clicked() {
                        s.cursor = Some(item.clone());
                        out.fold = Some(fold_key(inst_id, &["group", name]));
                        out.focus = Some(Zone::Main);
                    }
                    r
                }
                Item::File(rel) => {
                    let on = s.selected.as_ref() == Some(rel);
                    let file = rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let dot = if open_dirty.as_ref() == Some(rel) { "●  " } else { "" };
                    // A mod with one file: the mod's own row (the file's path is on hover).
                    let single = groups.iter().find(|g| g.files.len() == 1 && g.files[0].rel == *rel);
                    let r = match single {
                        Some(g) => {
                            let lead = match &g.icon {
                                Some(p) => Lead::Image(egui::ImageSource::Uri(format!("file://{}", p.display()).into())),
                                None => Lead::Glyph(if g.name == mc::OTHER { "\u{f0224}" } else { "\u{f107b}" }),
                            };
                            w::lead_row(ui, t, on, lead, &g.name, dot.trim(), None)
                        }
                        // Under its mod's heading, indented; the file's name is enough (the path is on hover).
                        None => {
                            let fmt = Format::of(rel).unwrap_or(Format::Text);
                            egui::Frame::new().inner_margin(egui::Margin { left: 18, ..Default::default() }).show(ui, |ui| w::lead_row(ui, t, on, Lead::Glyph(glyph(fmt)), &file, dot.trim(), None)).inner
                        }
                    }
                    .on_hover_text(rel.to_string_lossy());
                    if r.clicked() {
                        s.cursor = Some(item.clone());
                        out.pick = Some(rel.clone());
                        out.focus = Some(Zone::Main);
                    }
                    if r.double_clicked() {
                        out.focus = Some(Zone::Side);
                    }
                    r
                }
            };
            if at && focused {
                w::focus_mark(ui, t, r.rect);
            }
            if at && scroll {
                r.scroll_to_me(None);
            }
        }
    });
}

/// A mod with several config files: a fold arrow, its icon and name, and how many files.
fn group_header(ui: &mut Ui, t: &Theme, g: &Group, open: bool, at: bool) -> egui::Response {
    let size = t.body_size();
    let h = size + 18.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if ui.is_rect_visible(rect) {
        if at {
            ui.painter().rect_filled(rect, 0.0, t.fg_a(t.menu_selected_bg_alpha));
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, t.fg_a(t.hover_fill_alpha));
        }
        let cy = rect.center().y;
        let mut x = rect.left() + 8.0;
        let arrow = if open { "\u{f0140}" } else { "\u{f0142}" };
        ui.painter().text(egui::pos2(x, cy), egui::Align2::LEFT_CENTER, arrow, FontId::monospace(size), t.muted);
        x += size + 6.0;
        let icon_size = size + 4.0;
        match &g.icon {
            Some(p) => {
                let r = Rect::from_center_size(egui::pos2(x + icon_size / 2.0, cy), vec2(icon_size, icon_size));
                egui::Image::new(egui::ImageSource::Uri(format!("file://{}", p.display()).into())).corner_radius(2.0).paint_at(ui, r);
            }
            None => {
                let glyph = if g.name == mc::OTHER { "\u{f0224}" } else { "\u{f107b}" };
                ui.painter().text(egui::pos2(x, cy), egui::Align2::LEFT_CENTER, glyph, FontId::monospace(size), t.muted);
            }
        }
        x += icon_size + 8.0;
        let p = ui.painter();
        // Just the number: the mod's name needs the room.
        let c = p.layout_no_wrap(g.files.len().to_string(), FontId::monospace(size - 1.0), t.muted);
        let cw = c.size().x;
        p.galley(egui::pos2(rect.right() - 10.0 - cw, cy - c.size().y / 2.0), c, t.muted);
        let clip = Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 20.0 - cw, rect.bottom()));
        p.with_clip_rect(clip).text(egui::pos2(x, cy), egui::Align2::LEFT_CENTER, &g.name, FontId::monospace(size), if at { t.menu_selected_text } else { t.fg });
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// -- the editor -------------------------------------------------------------------

/// The shader pack open in the editor: whether it's the one in use, and why it can't be switched to if it can't.
struct PackSwitch {
    name: String,
    active: bool,
    why: Option<&'static str>,
    optifine: bool,
}

struct EditorState<'a> {
    /// For a shader pack: switching to it, on the status line.
    pack: Option<&'a PackSwitch>,
    /// Open settings headers ([`fold_key`]).
    folds: &'a BTreeSet<String>,
    field_filter: &'a mut String,
    field_sel: &'a mut Option<String>,
    scroll_field: &'a mut bool,
    unfolded: &'a mut HashSet<String>,
    /// The key binding waiting for a key, if any.
    capture: Option<&'a str>,
    /// A capture just ended (its click mustn't start another).
    cooling: bool,
}

#[allow(clippy::too_many_arguments)]
fn editor(ui: &mut Ui, t: &Theme, o: &mut Open, st: EditorState, running: bool, kb: Option<Nav>, focused: bool, out: &mut Out) {
    let dirty = o.dirty();
    let structured = o.format.structured();
    let key = |n: Nav| kb == Some(n);

    let title = |ui: &mut Ui| {
        ui.label(RichText::new(glyph(o.format)).color(t.muted));
        let dir = o.rel.parent().map(|p| p.to_string_lossy().into_owned()).filter(|p| !p.is_empty());
        let mut job = LayoutJob::default();
        if let Some(d) = dir {
            job.append(&format!("{d}/"), 0.0, TextFormat::simple(FontId::monospace(t.body_size()), t.muted));
        }
        job.append(&o.name(), 0.0, TextFormat::simple(theme::bold(t.body_size()), t.fg_light));
        ui.add(egui::Label::new(job).truncate());
    };
    // Buttons first, so a long name can't run under them. Read left to right: folder, restore defaults, fields/text,
    // revert, save.
    let labels = ["  save  ", "revert", "fields", "text", "restore defaults", "folder"];
    let need = w::buttons_width(ui, &labels) + 14.0;
    let mut actions = |ui: &mut Ui, ltr: bool| {
        let mut steps = [0, 1, 2, 3, 4];
        if ltr {
            steps.reverse();
        }
        for step in steps {
            // Wrapping can't see a group's width ahead (fields/text is two buttons): break the line first.
            if ltr {
                let width = match step {
                    0 => w::buttons_width(ui, &["  save  "]),
                    1 => w::buttons_width(ui, &["revert"]),
                    2 if structured => w::buttons_width(ui, &["fields", "text"]),
                    3 => w::buttons_width(ui, &["restore defaults"]),
                    4 => w::buttons_width(ui, &["folder"]),
                    _ => 0.0,
                };
                if width > 0.0 && ui.available_size_before_wrap().x < width && ui.cursor().min.x > ui.max_rect().min.x + 1.0 {
                    ui.end_row();
                }
            }
            match step {
                0 => {
                    if ui.add_enabled_ui(dirty, |ui| w::primary_button(ui, t, "  save  ")).inner.on_hover_text("ctrl+s").clicked() {
                        out.save = true;
                    }
                }
                1 => {
                    if ui.add_enabled(dirty, egui::Button::new("revert")).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() || (key(Nav::Delete) && dirty && o.raw_mode()) {
                        out.revert = true;
                    }
                }
                2 if structured => {
                    let mut raw = o.raw;
                    // Right to left, listed backwards; either way it reads "fields  text".
                    let opts = if ltr { [(false, "fields"), (true, "text")] } else { [(true, "text"), (false, "fields")] };
                    if w::segmented(ui, t, &mut raw, &opts) {
                        out.raw = Some(raw);
                    }
                }
                3 => {
                    // Everything with a known default back to it (nothing is written until save).
                    let differ = o.doc.as_ref().map_or(0, |d| d.fields.iter().filter(|f| default_value(f).is_some_and(|d| Value::Scalar(d) != f.value)).count());
                    let known = o.doc.as_ref().is_ok_and(|d| d.fields.iter().any(|f| default_value(f).is_some()));
                    let hover = if !known { "this file doesn't say what its defaults are".to_string() } else if differ == 0 { "everything is at its default".to_string() } else { format!("put {differ} settings back to their defaults; save to keep it") };
                    let r = ui.add_enabled(differ > 0 && !o.raw_mode(), egui::Button::new("restore defaults")).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if r.on_hover_text(&hover).on_disabled_hover_text(&hover).clicked() {
                        out.defaults = true;
                    }
                }
                4 => {
                    if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                        super::open_path(o.path.parent().unwrap_or(&o.path));
                    }
                }
                _ => {}
            }
        }
    };
    let room = ui.available_width();
    if room - need >= 280.0 {
        // Wide: the name beside the buttons.
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                actions(ui, false);
                ui.add_space(10.0);
                ui.with_layout(Layout::left_to_right(Align::Center), title);
            });
        });
    } else {
        // Narrow: the name on its own line, the buttons under it, wrapping when even they don't fit.
        ui.horizontal(title);
        ui.add_space(4.0);
        if room >= need {
            ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), |ui| actions(ui, false)));
        } else {
            ui.horizontal_wrapped(|ui| actions(ui, true));
        }
    }

    // One quiet status line: unsaved, saved, reloaded…
    ui.horizontal(|ui| {
        let size = t.body_size() - 1.0;
        let note = o.note.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4)).map(|(n, _)| n.clone());
        if dirty {
            ui.label(RichText::new("● unsaved changes").color(t.accent).size(size));
        } else if let Some(n) = &note {
            // A save of text with an error in it is a warning, not a tick.
            let (glyph, color) = if n.contains("error") { ("\u{f0026}", t.yellow) } else { ("\u{f012c}", t.green) };
            ui.label(RichText::new(format!("{glyph} {n}")).color(color).size(size));
        } else {
            ui.label(RichText::new("no changes").color(t.muted).size(size));
        }
        if dirty && let Some(n) = &note {
            ui.label(RichText::new(format!("·  {n}")).color(t.muted).size(size));
        }
        ui.label(RichText::new(format!("·  {}", o.format.label())).color(t.muted).size(size));
        if let Some(p) = st.pack {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if p.active {
                    let r = ui.add_enabled(p.why.is_none(), egui::Button::new("turn shaders off")).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if r.on_disabled_hover_text(p.why.unwrap_or_default()).clicked() {
                        out.use_pack = Some(None);
                    }
                } else {
                    let hover = if p.optifine { "make it OptiFine's shader pack" } else { "make it Iris's shader pack" };
                    let r = ui.add_enabled(p.why.is_none(), egui::Button::new("use this pack")).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(hover);
                    if r.on_disabled_hover_text(p.why.unwrap_or_default()).clicked() {
                        out.use_pack = Some(Some(p.name.clone()));
                    }
                }
            });
        }
        if note.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    });

    if running {
        ui.add_space(4.0);
        notice(ui, t, t.yellow, "\u{f05d6}", "The game is running. Most mods read their config when the game starts, and some write it back when it quits, so a change here may need a restart, or be overwritten when the game closes.", |_| {});
    }
    if o.conflict.is_some() {
        ui.add_space(4.0);
        notice(ui, t, t.orange, "\u{f04e6}", "This file changed on disk while you were editing it; the game rewrites its configs as it starts and quits. Keep your changes (saving replaces the new file), or take the file as it is now?", |ui| {
            ui.horizontal(|ui| {
                if w::button(ui, "keep my changes").clicked() {
                    out.keep_mine = true;
                }
                if w::button(ui, "use the file on disk").clicked() {
                    out.take_disk = true;
                }
            });
        });
    }
    ui.add_space(8.0);

    if o.raw_mode() {
        raw_editor(ui, t, o, key(Nav::Activate) || key(Nav::Edit), focused);
        return;
    }
    let doc = match &o.doc {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("This file doesn't read as {} ({e}), so it can only be edited as text for now.", o.format.label());
            notice(ui, t, t.red, "\u{f0026}", &msg, |ui| {
                if w::button(ui, "edit as text").clicked() {
                    out.raw = Some(true);
                }
            });
            return;
        }
    };
    if doc.fields.is_empty() {
        w::empty_state(ui, t, "\u{f0224}", "no settings to show as fields", "Switch to text to see and edit the whole file.");
        return;
    }

    // The filter, and how many settings it shows.
    let shown = shown_fields(doc, st.field_filter);
    ui.horizontal(|ui| {
        // The count first (right), then the filter in what's left, at most 320.
        let n = shown.len();
        let total = doc.fields.len();
        let text = if n == total { format!("{total} settings") } else { format!("{n} of {total} settings") };
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(text).color(t.muted));
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let width = (ui.available_width() - 16.0).clamp(60.0, 320.0);
                let r = w::text_input(ui, st.field_filter, "/ filter settings", width, key(Nav::Search));
                if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    st.field_filter.clear();
                    r.surrender_focus();
                }
            });
        });
    });
    ui.add_space(6.0);

    // The lines: settings outside any header first, then each header, folded unless opened (all open while filtering).
    let filtering = !st.field_filter.trim().is_empty();
    let fold_of = |section: &[String]| fold_key(&o.inst_id, &["section", o.area.key(), &o.rel.to_string_lossy(), &section.join("\u{1e}")]);
    enum Line<'d> {
        Header(&'d [String], usize, String),
        Field(usize, &'d Field, String),
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut last: Option<&[String]> = None;
    for (n, f) in &shown {
        let sec = f.section.as_slice();
        if !sec.is_empty() && last != Some(sec) {
            let count = shown.iter().filter(|(_, x)| x.section.as_slice() == sec).count();
            lines.push(Line::Header(sec, count, format!("§{}", sec.join("\u{1e}"))));
        }
        last = Some(sec);
        if sec.is_empty() || filtering || st.folds.contains(&fold_of(sec)) {
            lines.push(Line::Field(*n, f, field_id(f)));
        }
    }
    let id_of = |l: &Line| match l {
        Line::Header(_, _, id) | Line::Field(_, _, id) => id.clone(),
    };

    // Keyboard: move between headers and fields; enter or space on a header folds it.
    let ids: Vec<String> = lines.iter().map(id_of).collect();
    let mut pos = st.field_sel.as_ref().and_then(|id| ids.iter().position(|x| x == id));
    if pos.is_none() && !ids.is_empty() {
        pos = Some(0);
        *st.field_sel = Some(ids[0].clone());
    }
    if let Some(n) = kb.filter(|n| n.is_move())
        && let Some(p) = n.step(ids.len(), pos)
    {
        *st.field_sel = Some(ids[p].clone());
        pos = Some(p);
        *st.scroll_field = true;
    }
    if let Some(Line::Header(sec, _, _)) = pos.and_then(|p| lines.get(p))
        && (key(Nav::Activate) || key(Nav::Toggle))
    {
        out.fold = Some(fold_of(sec));
    }
    let on_field = pos.is_some_and(|p| matches!(lines.get(p), Some(Line::Field(..))));
    // Key bindings that share a key, by key: each one's row says what else is on it.
    let mut clashes: HashMap<&str, Vec<String>> = HashMap::new();
    for f in &doc.fields {
        if let Some(v) = binding_of(f).filter(|v| !v.ends_with(".unknown")) {
            clashes.entry(v).or_default().push(f.label.clone().unwrap_or_else(|| f.key.clone()));
        }
    }
    let focus_widget = on_field && (key(Nav::Activate) || key(Nav::Edit));
    let wide = ui.available_width() >= 560.0;
    let key_w = (ui.available_width() * 0.5).clamp(180.0, 440.0);
    let scroll = std::mem::take(st.scroll_field);

    {
        egui::ScrollArea::vertical().id_salt(("config-fields", &o.rel)).auto_shrink(false).show(ui, |ui| {
            for (row, line) in lines.iter().enumerate() {
                let selected = pos == Some(row);
                match line {
                    Line::Header(sec, count, id) => {
                        let open = filtering || st.folds.contains(&fold_of(sec));
                        ui.add_space(if row == 0 { 0.0 } else { 6.0 });
                        let r = section_header(ui, t, sec, *count, open, selected);
                        if selected && focused {
                            w::focus_mark(ui, t, r.rect);
                        }
                        if r.clicked() {
                            *st.field_sel = Some(id.clone());
                            out.fold = Some(fold_of(sec));
                            out.focus = Some(Zone::Side);
                        }
                        if selected && scroll {
                            ui.scroll_to_rect(r.rect, Some(Align::Center));
                        }
                        if open && let Some(d) = doc.section_doc(sec) {
                            egui::Frame::new().inner_margin(egui::Margin { left: 26, bottom: 4, top: 2, ..Default::default() }).show(ui, |ui| {
                                w::para(ui, RichText::new(d).color(t.muted).size(t.body_size() - 1.0));
                            });
                        }
                    }
                    Line::Field(n, f, id) => {
                        let changed = o.saved_values.get(id) != Some(&f.value);
                        let capturing = st.capture == Some(id.as_str());
                        let clash = binding_of(f).and_then(|v| clashes.get(v)).map(|names| names.iter().filter(|n| **n != f.label.as_deref().unwrap_or(&f.key)).cloned().collect()).unwrap_or_default();
                        let ctx = Row { t, id, selected, focused: selected && focused, changed, wide, key_w, capturing, clash, cooling: st.cooling };
                        let kb_here = if selected { kb } else { None };
                        // Settings under a header sit in from it.
                        let indent = if f.section.is_empty() { 0 } else { 18 };
                        let (rect, clicked, edit) = egui::Frame::new()
                            .inner_margin(egui::Margin { left: indent, ..Default::default() })
                            .show(ui, |ui| field_row(ui, &ctx, f, kb_here, selected && focus_widget, st.unfolded, out))
                            .inner;
                        if let Some(v) = edit {
                            out.edit = Some((*n, v));
                        }
                        if clicked {
                            *st.field_sel = Some(id.clone());
                            out.focus = Some(Zone::Side);
                        }
                        if selected && scroll {
                            ui.scroll_to_rect(rect, Some(Align::Center));
                        }
                    }
                }
            }
            ui.add_space(12.0);
        });
    }
}

/// A settings header: a fold arrow, the header's path and how many settings it holds, over a hairline.
fn section_header(ui: &mut Ui, t: &Theme, path: &[String], count: usize, open: bool, at: bool) -> egui::Response {
    let size = t.body_size();
    let h = size + 16.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if at {
            p.rect_filled(rect, 0.0, t.fg_a(t.menu_selected_bg_alpha));
        } else if resp.hovered() {
            p.rect_filled(rect, 0.0, t.fg_a(t.hover_fill_alpha));
        }
        let cy = rect.center().y;
        let arrow = if open { "\u{f0140}" } else { "\u{f0142}" };
        p.text(egui::pos2(rect.left() + 6.0, cy), egui::Align2::LEFT_CENTER, arrow, FontId::monospace(size), t.muted);
        let x = rect.left() + 6.0 + size + 6.0;
        let c = p.layout_no_wrap(if count == 1 { "1 setting".into() } else { format!("{count} settings") }, FontId::monospace(size - 1.0), t.muted);
        let cw = c.size().x;
        p.galley(egui::pos2(rect.right() - 8.0 - cw, cy - c.size().y / 2.0), c, t.muted);
        // The screens it's inside, quietly; its own name in bold.
        let mut job = LayoutJob::default();
        if let Some((last, parents)) = path.split_last() {
            for s in parents {
                job.append(&format!("{s}  ›  "), 0.0, TextFormat::simple(FontId::monospace(size), t.muted));
            }
            job.append(last, 0.0, TextFormat::simple(theme::bold(size), if at { t.menu_selected_text } else { t.fg_light }));
        }
        let g = p.layout_job(job);
        let tw = g.size().x;
        let clip = Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 18.0 - cw, rect.bottom()));
        p.with_clip_rect(clip).galley(egui::pos2(x, cy - g.size().y / 2.0), g, t.fg_light);
        let (l, r) = (x + tw + 10.0, rect.right() - 18.0 - cw);
        if r > l {
            p.hline(l..=r, cy, Stroke::new(1.0, t.fg_a(0.10)));
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A bordered note with a coloured glyph, and room for buttons under the text.
fn notice(ui: &mut Ui, t: &Theme, color: egui::Color32, glyph: &str, text: &str, extra: impl FnOnce(&mut Ui)) {
    egui::Frame::new().stroke(Stroke::new(1.0, Theme::alpha(color, 0.55))).fill(Theme::alpha(color, 0.06)).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(glyph).color(color));
            ui.vertical(|ui| {
                w::para(ui, RichText::new(text).color(t.fg).size(t.body_size() - 1.0));
                extra(ui);
            });
        });
    });
}

struct Row<'a> {
    t: &'a Theme,
    id: &'a str,
    selected: bool,
    focused: bool,
    changed: bool,
    wide: bool,
    key_w: f32,
    /// Key bindings: waiting for this one's key, the other bindings on the same key, and whether a capture just
    /// ended.
    capturing: bool,
    clash: Vec<String>,
    cooling: bool,
}

/// One setting: its key and description on the left, its value on the right (or under
/// each other when narrow). Returns its rect, whether it was clicked, and a new value.
fn field_row(ui: &mut Ui, r: &Row, f: &Field, kb: Option<Nav>, focus_widget: bool, unfolded: &mut HashSet<String>, out: &mut Out) -> (Rect, bool, Option<Value>) {
    let t = r.t;
    let bg = ui.painter().add(egui::Shape::Noop);
    let mut edit = None;
    let inner = egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 10, top: 8, bottom: 9 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let key_part = |ui: &mut Ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if r.changed {
                    ui.label(RichText::new("●").color(t.accent).size(t.body_size() - 3.0)).on_hover_text("changed, not saved yet");
                }
                let color = if r.selected { t.fg_light } else { t.fg };
                let name = f.label.as_deref().unwrap_or(&f.key);
                ui.add(egui::Label::new(RichText::new(name).font(theme::bold(t.body_size())).color(color)).truncate()).on_hover_text(&f.key);
            });
            if !f.doc.is_empty() {
                w::para(ui, RichText::new(&f.doc).color(t.muted).size(t.body_size() - 1.0));
            }
        };
        if r.wide {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(vec2(r.key_w, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(r.key_w);
                    key_part(ui);
                });
                ui.add_space(18.0);
                ui.vertical(|ui| edit = value_part(ui, t, f, r, kb, focus_widget, unfolded, out));
            });
        } else {
            key_part(ui);
            ui.add_space(4.0);
            edit = value_part(ui, t, f, r, kb, focus_widget, unfolded, out);
        }
    });
    let rect = inner.response.rect;
    let hovered = ui.rect_contains_pointer(rect);
    let clicked = hovered && ui.input(|i| i.pointer.primary_pressed());
    let shape = if r.selected {
        egui::Shape::Rect(egui::epaint::RectShape::new(rect, CornerRadius::ZERO, t.fg_a(t.menu_selected_bg_alpha * 0.7), Stroke::new(1.0, t.fg_a(t.menu_selected_border_alpha * 0.6)), StrokeKind::Inside))
    } else if hovered {
        egui::Shape::rect_filled(rect, CornerRadius::ZERO, t.fg_a(t.hover_fill_alpha * 0.35))
    } else {
        egui::Shape::Noop
    };
    ui.painter().set(bg, shape);
    if r.focused {
        w::focus_mark(ui, t, rect);
    }
    (rect, clicked, edit)
}

/// A key binding's key, as written (`key.keyboard.w`).
fn binding_of(f: &Field) -> Option<&str> {
    match &f.value {
        Value::Scalar(Scalar::Str(v)) if f.hint.binding => Some(v),
        _ => None,
    }
}

/// Minecraft's name for a key (GLFW's, as `options.txt` writes it).
fn mc_key(k: egui::Key) -> Option<String> {
    use egui::Key::*;
    let name = match k {
        Space => "space",
        Enter => "enter",
        Tab => "tab",
        Backspace => "backspace",
        Insert => "insert",
        Delete => "delete",
        Home => "home",
        End => "end",
        PageUp => "page.up",
        PageDown => "page.down",
        ArrowUp => "up",
        ArrowDown => "down",
        ArrowLeft => "left",
        ArrowRight => "right",
        Minus => "minus",
        Equals | Plus => "equal",
        Comma => "comma",
        Period => "period",
        Slash => "slash",
        Backslash => "backslash",
        Semicolon | Colon => "semicolon",
        Quote => "apostrophe",
        OpenBracket => "left.bracket",
        CloseBracket => "right.bracket",
        Backtick => "grave.accent",
        _ => {
            let n = k.name();
            // Letters, digits ("1" or "Num1") and F-keys.
            let n = n.strip_prefix("Num").unwrap_or(n);
            return (n.len() == 1 && n.chars().all(|c| c.is_ascii_alphanumeric()) || (n.starts_with('F') && n[1..].parse::<u8>().is_ok())).then(|| format!("key.keyboard.{}", n.to_ascii_lowercase()));
        }
    };
    Some(format!("key.keyboard.{name}"))
}

/// While a binding waits for its key: take the next key, mouse button or newly held shift/ctrl/alt.
pub fn capture(app: &mut App, ctx: &egui::Context) {
    let Some(id) = app.config.capture.clone() else { return };
    let Some(before) = app.config.capture_mods else {
        app.config.capture_mods = Some(ctx.input(|i| i.modifiers));
        return;
    };
    let (event, mods) = ctx.input_mut(|i| {
        let n = i.events.iter().position(|e| matches!(e, egui::Event::Key { pressed: true, .. } | egui::Event::PointerButton { pressed: true, .. }));
        (n.map(|n| i.events.remove(n)), i.modifiers)
    });
    let value = match event {
        Some(egui::Event::Key { key: egui::Key::Escape, .. }) => None,
        Some(egui::Event::Key { key, .. }) => mc_key(key),
        Some(egui::Event::PointerButton { button, .. }) => Some(
            match button {
                egui::PointerButton::Primary => "key.mouse.left",
                egui::PointerButton::Secondary => "key.mouse.right",
                egui::PointerButton::Middle => "key.mouse.middle",
                egui::PointerButton::Extra1 => "key.mouse.4",
                egui::PointerButton::Extra2 => "key.mouse.5",
            }
            .to_string(),
        ),
        _ => match () {
            _ if mods.shift && !before.shift => Some("key.keyboard.left.shift".into()),
            _ if mods.ctrl && !before.ctrl => Some("key.keyboard.left.control".into()),
            _ if mods.alt && !before.alt => Some("key.keyboard.left.alt".into()),
            _ => return,
        },
    };
    app.config.capture = None;
    app.config.capture_ended = Some(Instant::now());
    // esc (or a key Minecraft has no name for) leaves the binding as it was.
    if let Some(v) = value {
        app.config.captured = Some((id, v));
    }
}

/// "1 – 64", "at least 0"…; nothing for the type's own limits (Forge writes
/// `Range: > -2147483648` for "any int").
fn range_text(h: &mc::Hint) -> Option<String> {
    let n = |v: f64| if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") };
    let limit = |v: f64| v.abs() >= 2147483647.0;
    match (h.min.filter(|v| !limit(*v)), h.max.filter(|v| !limit(*v))) {
        (Some(a), Some(b)) => Some(format!("{} – {}", n(a), n(b))),
        (Some(a), None) => Some(format!("at least {}", n(a))),
        (None, Some(b)) => Some(format!("at most {}", n(b))),
        _ => None,
    }
}

/// The default from the comment as a value of this field's type.
fn default_value(f: &Field) -> Option<Scalar> {
    let d = f.hint.default.as_deref()?;
    Some(match &f.value {
        Value::Scalar(Scalar::Bool(_)) => Scalar::Bool(d.parse().ok()?),
        Value::Scalar(Scalar::Int(_)) => Scalar::Int(d.parse().ok()?),
        Value::Scalar(Scalar::Float(_)) => Scalar::Float(d.parse().ok()?),
        Value::Scalar(Scalar::Str(_)) => Scalar::Str(d.to_string()),
        _ => return None,
    })
}

/// The value's widget and, under it, what the comment says about it.
#[allow(clippy::too_many_arguments)]
fn value_part(ui: &mut Ui, t: &Theme, f: &Field, r: &Row, kb: Option<Nav>, focus: bool, unfolded: &mut HashSet<String>, out: &mut Out) -> Option<Value> {
    let id = r.id;
    let key = |n: Nav| kb == Some(n);
    let wid = egui::Id::new(("config-field", id));
    let mut edit = None;
    match &f.value {
        // A key binding: click (or enter), then press the key; right-click unbinds. Like Omacraft's own keys.
        Value::Scalar(Scalar::Str(v)) if f.hint.binding => {
            let shown = crate::vanilla_options::binding_value(v).unwrap_or_else(|| v.clone());
            let text = if r.capturing { "press a key…".to_string() } else { shown };
            let color = if r.capturing { t.accent } else if !r.clash.is_empty() { t.yellow } else { t.fg };
            let mut b = egui::Button::new(RichText::new(text).color(color)).min_size(vec2(160.0, 0.0));
            if r.capturing {
                b = b.stroke(Stroke::new(1.0, t.accent));
            }
            let hover = if r.capturing {
                "press a key or mouse button · esc cancels".to_string()
            } else if r.clash.is_empty() {
                "click to change, right-click to unbind".to_string()
            } else {
                format!("also bound to: {}", r.clash.join(", "))
            };
            let resp = ui.add(b).on_hover_text(hover).on_hover_cursor(egui::CursorIcon::PointingHand);
            if ((resp.clicked() && !r.cooling) || focus) && !r.capturing {
                out.capture = Some(id.to_string());
            }
            if resp.secondary_clicked() {
                edit = Some(Value::Scalar(Scalar::Str("key.keyboard.unknown".into())));
            }
        }
        Value::Scalar(s) => {
            if let Some(v) = scalar_widget(ui, t, s, &f.hint, wid, kb, focus, ui.available_width()) {
                edit = Some(Value::Scalar(v));
            }
        }
        Value::List { items, editable: true } => {
            if let Some(items) = list_widget(ui, t, items, wid, id, unfolded, focus) {
                edit = Some(Value::List { items, editable: true });
            }
        }
        Value::List { .. } | Value::Other(_) => {
            let preview = match &f.value {
                Value::List { items, .. } => format!("{} items", items.len()),
                Value::Other(raw) => raw.lines().next().unwrap_or_default().chars().take(60).collect(),
                _ => String::new(),
            };
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(preview).color(t.muted).monospace()).truncate());
            });
            if ui.add(egui::Label::new(RichText::new("edit as text").color(t.accent).size(t.body_size() - 1.0)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() || key(Nav::Activate) {
                out.raw = Some(true);
            }
        }
    }

    // Range and default, under the widget.
    let range = range_text(&f.hint);
    let default = default_value(f);
    let current = if let Value::Scalar(s) = &f.value { Some(s) } else { None };
    let at_default = default.as_ref().is_some_and(|d| Some(d) == current);
    if f.hint.allowed.len() == 1 {
        return edit;
    }
    if range.is_some() || default.is_some() {
        ui.horizontal(|ui| {
            let size = t.body_size() - 1.0;
            ui.spacing_mut().item_spacing.x = 4.0;
            if let Some(r) = &range {
                ui.label(RichText::new(r).color(t.muted).size(size));
            }
            if let Some(d) = &default {
                if range.is_some() {
                    ui.label(RichText::new("·").color(t.muted).size(size));
                }
                let shown = match d {
                    Scalar::Bool(b) if f.hint.on_off => (if *b { "on" } else { "off" }).to_string(),
                    Scalar::Str(v) if f.hint.binding => crate::vanilla_options::binding_value(v).unwrap_or_else(|| v.clone()),
                    _ => f.hint.name_of(&d.display()).to_string(),
                };
                let text = format!("default {shown}");
                if at_default {
                    ui.label(RichText::new(text).color(t.muted).size(size));
                } else {
                    let r = ui.add(egui::Label::new(RichText::new(format!("{text} \u{f054c}")).color(t.muted).size(size)).sense(Sense::click()));
                    let r = r.on_hover_text("put the default back (x)").on_hover_cursor(egui::CursorIcon::PointingHand);
                    if r.clicked() || key(Nav::Delete) {
                        edit = Some(Value::Scalar(d.clone()));
                    }
                }
            }
        });
    }
    edit
}

/// An on/off switch: square, like the rest.
fn switch(ui: &mut Ui, t: &Theme, on: bool) -> egui::Response {
    let h = t.body_size() + 2.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(h * 2.0, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let a = ui.ctx().animate_bool_with_time(resp.id, on, 0.1);
        let p = ui.painter();
        let border = if on { t.accent } else if resp.hovered() { t.fg_a(0.5) } else { t.fg_a(0.3) };
        p.rect(rect, CornerRadius::ZERO, Theme::alpha(t.accent, 0.2 * a), Stroke::new(1.0, border), StrokeKind::Inside);
        let k = h - 6.0;
        let x = egui::lerp((rect.left() + 3.0)..=(rect.right() - 3.0 - k), a);
        let knob = if on { t.accent } else { t.muted };
        p.rect_filled(Rect::from_min_size(egui::pos2(x, rect.top() + 3.0), vec2(k, k)), CornerRadius::ZERO, knob);
        if resp.has_focus() {
            p.rect_stroke(rect.expand(2.0), CornerRadius::ZERO, Stroke::new(1.0, t.fg_a(0.4)), StrokeKind::Outside);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The widget for one value; returns the new value when it changes. `kb` is the
/// keyboard's action on it, `focus` asks for it to take the keyboard.
fn scalar_widget(ui: &mut Ui, t: &Theme, s: &Scalar, hint: &mc::Hint, wid: egui::Id, kb: Option<Nav>, focus: bool, max_w: f32) -> Option<Scalar> {
    let key = |n: Nav| kb == Some(n);
    let lo = hint.min;
    let hi = hint.max;
    let narrow_range = |lo: f64, hi: f64| hi > lo && hi - lo <= 1000.0;
    match s {
        Scalar::Bool(b) => {
            let mut out = None;
            ui.horizontal(|ui| {
                let r = switch(ui, t, *b);
                let word = match (hint.on_off, *b) {
                    (true, true) => "on",
                    (true, false) => "off",
                    (false, true) => "true",
                    (false, false) => "false",
                };
                let label = ui.add(egui::Label::new(RichText::new(word).color(if *b { t.fg } else { t.muted })).sense(Sense::click()));
                if r.clicked() || label.clicked() || key(Nav::Toggle) || focus {
                    out = Some(Scalar::Bool(!b));
                }
            });
            out
        }
        Scalar::Int(v) => {
            let mut x = *v;
            let (a, b) = (lo.map_or(i64::MIN, |m| m.max(i64::MIN as f64) as i64), hi.map_or(i64::MAX, |m| m.min(i64::MAX as f64) as i64));
            let r = match (lo, hi) {
                (Some(l), Some(h)) if narrow_range(l, h) => {
                    ui.spacing_mut().slider_width = (ui.available_width() - 90.0).clamp(80.0, 240.0);
                    ui.add(egui::Slider::new(&mut x, a..=b).clamping(egui::SliderClamping::Edits))
                }
                _ => ui.add(egui::DragValue::new(&mut x).range(a..=b).clamp_existing_to_range(false).speed(((v.abs() as f64) * 0.01).max(0.1))),
            };
            if focus {
                r.request_focus();
            }
            (r.changed() && x != *v).then_some(Scalar::Int(x))
        }
        Scalar::Float(v) => {
            let mut x = *v;
            let (a, b) = (lo.unwrap_or(f64::MIN), hi.unwrap_or(f64::MAX));
            let r = match (lo, hi) {
                (Some(l), Some(h)) if narrow_range(l, h) => {
                    ui.spacing_mut().slider_width = (ui.available_width() - 90.0).clamp(80.0, 240.0);
                    ui.add(egui::Slider::new(&mut x, a..=b).clamping(egui::SliderClamping::Edits))
                }
                _ => ui.add(egui::DragValue::new(&mut x).range(a..=b).clamp_existing_to_range(false).speed((v.abs() * 0.01).max(0.01))),
            };
            if focus {
                r.request_focus();
            }
            (r.changed() && x != *v).then_some(Scalar::Float(x))
        }
        // A pack's information "button": one value, nothing to choose. Its name and text are the point.
        Scalar::Str(_) if hint.allowed.len() == 1 => None,
        // Values along a scale (a shader's sliders): a slider over the steps, showing the step's name.
        Scalar::Str(v) if hint.steps && hint.allowed.len() > 1 => {
            let steps = &hint.allowed;
            let at = steps.iter().position(|x| x == v);
            let mut i = at.unwrap_or(0);
            let mut pick = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 110.0).clamp(80.0, 240.0);
                let r = ui.add(egui::Slider::new(&mut i, 0..=steps.len() - 1).show_value(false));
                if focus {
                    r.request_focus();
                }
                if r.changed() && Some(i) != at {
                    pick = Some(steps[i].clone());
                }
                ui.label(RichText::new(hint.name_of(v)).color(t.fg));
            });
            if key(Nav::Toggle) {
                pick = Some(steps[(at.map_or(0, |a| a + 1)) % steps.len()].clone());
            }
            pick.map(Scalar::Str)
        }
        Scalar::Str(v) if !hint.allowed.is_empty() => {
            let mut options = hint.allowed.clone();
            if !options.contains(v) {
                options.insert(0, v.clone());
            }
            let mut pick = None;
            // Few short choices read best side by side; more go in a menu.
            let short = options.len() <= 4 && options.iter().map(|o| hint.name_of(o).chars().count() + 3).sum::<usize>() as f32 * t.body_size() * 0.62 < ui.available_width();
            if short {
                let mut cur = v.clone();
                let opts: Vec<(String, &str)> = options.iter().map(|o| (o.clone(), hint.name_of(o))).collect();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (val, label) in &opts {
                        let on = cur == *val;
                        let text = if on { RichText::new(*label).color(t.bg) } else { RichText::new(*label).color(t.fg) };
                        let mut b = egui::Button::new(text).corner_radius(0);
                        if on {
                            b = b.fill(t.accent).stroke(Stroke::new(1.0, t.accent));
                        }
                        if ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                            cur = val.clone();
                            pick = Some(cur.clone());
                        }
                    }
                });
            } else {
                let width = (max_w - 8.0).clamp(120.0, 280.0);
                egui::ComboBox::from_id_salt(wid).selected_text(hint.name_of(v)).width(width).show_ui(ui, |ui| {
                    for o in &options {
                        if ui.selectable_label(o == v, hint.name_of(o)).clicked() && o != v {
                            pick = Some(o.clone());
                        }
                    }
                });
            }
            if key(Nav::Toggle) || focus {
                let i = options.iter().position(|o| o == v).unwrap_or(0);
                pick = Some(options[(i + 1) % options.len()].clone());
            }
            pick.map(Scalar::Str)
        }
        Scalar::Str(v) => {
            let mut buf = v.clone();
            let width = (max_w - 4.0).clamp(100.0, 420.0);
            let r = ui.add(egui::TextEdit::singleline(&mut buf).id_salt(wid).desired_width(width).margin(vec2(8.0, 4.0)).font(egui::TextStyle::Monospace));
            if focus {
                r.request_focus();
            }
            (r.changed() && buf != *v).then_some(Scalar::Str(buf))
        }
    }
}

/// A list of plain values: one row each, with remove and add. Long ones start folded.
fn list_widget(ui: &mut Ui, t: &Theme, items: &[Scalar], wid: egui::Id, id: &str, unfolded: &mut HashSet<String>, focus: bool) -> Option<Vec<Scalar>> {
    let mut new: Option<Vec<Scalar>> = None;
    let long = items.len() > FOLD_AT;
    let open = !long || unfolded.contains(id);
    if long {
        let label = if open { format!("{} items  ·  fold", items.len()) } else { format!("{} items  ·  show", items.len()) };
        if ui.add(egui::Label::new(RichText::new(label).color(t.accent)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() || (focus && !open) {
            if open {
                unfolded.remove(id);
            } else {
                unfolded.insert(id.to_string());
            }
        }
    }
    if !open {
        let preview: Vec<String> = items.iter().take(4).map(Scalar::display).collect();
        ui.add(egui::Label::new(RichText::new(format!("{} …", preview.join(", "))).color(t.muted).size(t.body_size() - 1.0)).truncate());
        return None;
    }
    if items.is_empty() {
        ui.label(RichText::new("empty").color(t.muted).size(t.body_size() - 1.0));
    }
    let hint = mc::Hint::default();
    // Every row the same width, with room for its remove button.
    let row_w = ui.available_width() - t.body_size() * 2.5;
    for (i, it) in items.iter().enumerate() {
        ui.horizontal(|ui| {
            let changed = scalar_widget(ui, t, it, &hint, wid.with(i), None, focus && i == 0, row_w);
            if let Some(v) = changed {
                let mut v2 = items.to_vec();
                v2[i] = v;
                new = Some(v2);
            }
            if ui.add(egui::Button::new(RichText::new("\u{f0156}").color(t.muted)).frame(false)).on_hover_text("remove").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                let mut v2 = items.to_vec();
                v2.remove(i);
                new = Some(v2);
            }
        });
    }
    if ui.add(egui::Label::new(RichText::new("+ add").color(t.accent).size(t.body_size() - 1.0)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        let blank = match items.first() {
            Some(Scalar::Bool(_)) => Scalar::Bool(false),
            Some(Scalar::Int(_)) => Scalar::Int(0),
            Some(Scalar::Float(_)) => Scalar::Float(0.0),
            _ => Scalar::Str(String::new()),
        };
        let mut v2 = items.to_vec();
        v2.push(blank);
        new = Some(v2);
    }
    new
}

// -- text -------------------------------------------------------------------------

fn raw_editor(ui: &mut Ui, t: &Theme, o: &mut Open, focus: bool, focused: bool) {
    let size = t.body_size() - 1.0;
    let font = FontId::monospace(size);
    let error = if o.format.structured() { o.doc.as_ref().err().cloned() } else { None };
    // The error line goes at the bottom; the editor takes the rest.
    let bottom = if error.is_some() { t.body_size() * 2.6 } else { 0.0 };
    let stroke = if focused { Stroke::new(1.0, t.accent) } else { Stroke::new(1.0, t.fg_a(0.10)) };
    let height = (ui.available_height() - bottom).max(120.0);
    let mut changed = false;
    egui::Frame::new().fill(t.bg_dark).stroke(stroke).inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
        ui.set_height(height - 12.0);
        egui::ScrollArea::both().id_salt(("config-text", &o.rel)).auto_shrink(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                // Line numbers, the same height as the text's lines.
                let lines = o.text.split('\n').count().max(1);
                let digits = lines.to_string().len();
                let numbers: String = (1..=lines).map(|n| format!("{n:>digits$}")).collect::<Vec<_>>().join("\n");
                let err_line = error.as_ref().map(|e| e.line);
                let mut job = LayoutJob::default();
                for (n, l) in numbers.split('\n').enumerate() {
                    let c = if err_line == Some(n + 1) { t.red } else { t.fg_a(0.28) };
                    job.append(l, 0.0, TextFormat::simple(font.clone(), c));
                    if n + 1 < lines {
                        job.append("\n", 0.0, TextFormat::simple(font.clone(), c));
                    }
                }
                ui.add(egui::Label::new(job).selectable(false));
                let format = o.format;
                let theme = t.clone();
                let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    let mut job = highlight(&theme, format, buf.as_str(), size);
                    job.wrap.max_width = f32::INFINITY;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let edit = egui::TextEdit::multiline(&mut o.text)
                    .id_salt(("config-text-edit", &o.rel))
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .margin(vec2(0.0, 0.0))
                    .desired_width(f32::INFINITY)
                    .desired_rows(lines.max(10))
                    .lock_focus(true)
                    .layouter(&mut layouter);
                let r = ui.add(edit);
                if focus {
                    r.request_focus();
                }
                changed = r.changed();
            });
        });
    });
    if changed {
        o.doc = mc::parse(o.format, &o.text);
    }
    if let Some(e) = error {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("\u{f0026}").color(t.red));
            ui.add(egui::Label::new(RichText::new(format!("{e}")).color(t.red).size(t.body_size() - 1.0)).truncate());
        });
    }
}

/// Colours for config text: comments, keys, strings, numbers, section headers.
fn highlight(t: &Theme, format: Format, text: &str, size: f32) -> LayoutJob {
    let font = FontId::monospace(size);
    let mut job = LayoutJob::default();
    if text.len() > HIGHLIGHT_MAX {
        job.append(text, 0.0, TextFormat::simple(font, t.fg));
        return job;
    }
    let mut push = |range: std::ops::Range<usize>, c: egui::Color32| {
        if !range.is_empty() {
            job.append(&text[range], 0.0, TextFormat::simple(font.clone(), c));
        }
    };
    let b = text.as_bytes();
    let string_end = |from: usize, q: u8| {
        let mut j = from + 1;
        while j < b.len() && b[j] != q && b[j] != b'\n' {
            if b[j] == b'\\' && !(format == Format::Toml && q == b'\'') {
                j += 1;
            }
            j += 1;
        }
        (j + 1).min(b.len())
    };
    let word_end = |from: usize| {
        let mut j = from;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'.' | b'+' | b'-' | b'_')) {
            j += 1;
        }
        j
    };
    let is_value_word = |w: &str| matches!(w, "true" | "false" | "null" | "inf" | "nan" | "Infinity" | "NaN") || w.trim_start_matches(['+', '-']).starts_with(|c: char| c.is_ascii_digit() || c == '.');
    match format {
        Format::Json | Format::Json5 => {
            let mut i = 0;
            let mut plain = 0;
            while i < b.len() {
                let (end, color) = match b[i] {
                    b'/' if b.get(i + 1) == Some(&b'/') => (text[i..].find('\n').map_or(b.len(), |n| i + n), t.muted),
                    b'/' if b.get(i + 1) == Some(&b'*') => (text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2), t.muted),
                    q @ (b'"' | b'\'') => {
                        let end = string_end(i, q);
                        let after = text[end..].trim_start_matches([' ', '\t']);
                        (end, if after.starts_with(':') { t.blue } else { t.green })
                    }
                    c if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.' | b'_') => {
                        let end = word_end(i);
                        let w = &text[i..end];
                        let after = text[end..].trim_start_matches([' ', '\t']);
                        if after.starts_with(':') {
                            (end, t.blue)
                        } else if is_value_word(w) {
                            (end, t.orange)
                        } else {
                            (end, t.fg)
                        }
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                push(plain..i, t.fg_a(0.7));
                push(i..end, color);
                i = end;
                plain = end;
            }
            push(plain..b.len(), t.fg_a(0.7));
        }
        Format::Toml | Format::Text | Format::Options | Format::ShaderSettings => {
            let mut start = 0;
            for line in text.split_inclusive('\n') {
                let end = start + line.len();
                let trimmed = line.trim_start();
                let lead = start + (line.len() - trimmed.len());
                push(start..lead, t.fg);
                let comment_start = |c: u8| c == b'#' || (format != Format::Toml && (c == b';' || c == b'!'));
                if trimmed.bytes().next().is_some_and(comment_start) || trimmed.starts_with("//") {
                    push(lead..end, t.muted);
                } else if trimmed.starts_with('[') {
                    push(lead..end, t.accent);
                } else {
                    // key = value  (or key: value in .properties and YAML)
                    let sep = line[lead - start..].find(['=', ':']).map(|n| lead + n);
                    let mut i = lead;
                    if let Some(s) = sep {
                        push(lead..s, t.blue);
                        push(s..s + 1, t.fg_a(0.7));
                        i = s + 1;
                    }
                    let mut plain = i;
                    while i < end {
                        let c = b[i];
                        let (stop, color) = if c == b'#' && format == Format::Toml {
                            (end, t.muted)
                        } else if c == b'"' || c == b'\'' {
                            (string_end(i, c).min(end), t.green)
                        } else if (c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.')) && (i == plain || !b[i - 1].is_ascii_alphanumeric()) {
                            let we = word_end(i).min(end);
                            (we, if is_value_word(&text[i..we]) { t.orange } else { t.fg })
                        } else {
                            i += 1;
                            continue;
                        };
                        push(plain..i, t.fg_a(0.7));
                        push(i..stop, color);
                        i = stop;
                        plain = stop;
                    }
                    push(plain..end, t.fg_a(0.7));
                }
                start = end;
            }
        }
    }
    job
}

// -- leaving with unsaved changes ---------------------------------------------------

/// Before the view moves off a file with unsaved changes (another instance, another
/// tab, quitting), put it back and ask. Run once a frame, after the panes.
pub fn guard(app: &mut App, ctx: &egui::Context) {
    let Some(o) = &app.config.open else { return };
    if !o.dirty() {
        return;
    }
    let inst_id = o.inst_id.clone();
    if !app.instances.iter().any(|i| i.id == inst_id) {
        app.config.open = None;
        return;
    }
    let back = |app: &mut App| {
        app.tab = Tab::Instances;
        app.inst_tab = InstTab::Config;
        app.selected = Some(inst_id.clone());
    };
    if ctx.input(|i| i.viewport().close_requested()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if app.modal.is_none() {
            back(app);
            app.modal = Some(Modal::ConfigLeave(Box::new(Leave { away: Away::Quit })));
        }
        return;
    }
    let here = app.tab == Tab::Instances && app.inst_tab == InstTab::Config && app.selected.as_deref() == Some(inst_id.as_str());
    if here || app.modal.is_some() {
        return;
    }
    let away = Away::View { tab: app.tab, inst_tab: app.inst_tab, selected: app.selected.clone() };
    back(app);
    app.modal = Some(Modal::ConfigLeave(Box::new(Leave { away })));
}

/// Go where the user was heading.
fn go(app: &mut App, away: &Away, ctx: &egui::Context) {
    match away {
        Away::File(area, rel) => {
            app.config.area = *area;
            app.config.selected = Some(rel.clone());
            app.config.field_sel = None;
            app.config.field_filter.clear();
        }
        Away::View { tab, inst_tab, selected } => {
            app.tab = *tab;
            app.inst_tab = *inst_tab;
            app.selected = selected.clone();
        }
        Away::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
    }
}

/// The "unsaved changes" dialog; true when it's done.
pub fn leave_dialog(app: &mut App, s: &Leave, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let nav = app.modal_nav();
    let Some(o) = &app.config.open else { return true };
    let name = o.rel.display().to_string();
    ui.set_width(440.0);
    w::heading(ui, &t, "unsaved changes");
    ui.add_space(8.0);
    w::para(ui, RichText::new(format!("{name} has changes that aren't saved yet.")).color(t.fg));
    ui.add_space(14.0);
    enum Choice {
        Save,
        Discard,
        Stay,
    }
    let mut choice = None;
    ui.horizontal(|ui| {
        if w::primary_button(ui, &t, "save").clicked() || nav == Some(Nav::Activate) {
            choice = Some(Choice::Save);
        }
        if w::danger_button(ui, &t, "discard").clicked() {
            choice = Some(Choice::Discard);
        }
        if w::button(ui, "keep editing").clicked() {
            choice = Some(Choice::Stay);
        }
    });
    let ctx = ui.ctx().clone();
    match choice {
        // If it can't be saved (it changed on disk, or an error), stay: the editor says why.
        Some(Choice::Save) => {
            if save(app, false) {
                go(app, &s.away, &ctx);
            }
            true
        }
        Some(Choice::Discard) => {
            app.config.open = None;
            go(app, &s.away, &ctx);
            true
        }
        Some(Choice::Stay) => true,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighting_keeps_every_byte() {
        let t = Theme::default();
        for (format, text) in [
            (Format::Toml, "# c\n[a]\nk = \"v # not\" # yes\nn = 1.5\nlist = ['x', 2]\n"),
            (Format::Json5, "{\n  // c\n  'k': \"v\", n: -1e3, /* b */ x: [true, null]\n}"),
            (Format::Text, "; c\nkey=value\n[s]\nb: true\nunterminated = \"abc"),
        ] {
            let job = highlight(&t, format, text, 12.0);
            assert_eq!(job.text, text, "{format:?}");
        }
    }

    /// The check for a file rewritten on disk reads and parses it off the UI thread: the old text stays
    /// shown until its result is taken, and a result for a file since reloaded, saved or swapped for
    /// another is dropped.
    #[test]
    fn disk_checks_apply_only_to_the_file_they_were_made_of() {
        let dir = crate::paths::test_root().join(format!("config-recheck-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let open = |text: &str| Open::new("inst".into(), Area::Mods, "a.toml".into(), mc::Loaded { stamp: mc::Stamp::of(&path), format: Format::Toml, base: None, path: path.clone(), text: text.into() });
        let a_is = |o: &Open| o.doc.as_ref().unwrap().fields[0].value.clone();
        let mut o = open("a = 1\n");
        // Nothing changed: nothing is read.
        let r = reread(&o.path, o.stamp, o.format, None);
        assert!(r.changed.is_none());
        assert!(o.take_reread(&o.check_of(), r));
        // The game rewrites it: the check finds it, but the editor shows the old file until it's taken.
        std::fs::write(&path, "a = 22\n").unwrap();
        let of = o.check_of();
        let r = reread(&o.path, o.stamp, o.format, None);
        assert_eq!(o.text, "a = 1\n");
        assert!(o.take_reread(&of, r));
        assert_eq!((o.text.as_str(), o.saved.as_str()), ("a = 22\n", "a = 22\n"));
        assert_eq!(a_is(&o), Value::Scalar(Scalar::Int(22)));
        // A check made before the file was reloaded (or saved) is stale: dropped.
        std::fs::write(&path, "a = 333\n").unwrap();
        let of = o.check_of();
        let r = reread(&o.path, o.stamp, o.format, None);
        o.reload("a = 4\n".into());
        assert!(!o.take_reread(&of, r));
        assert_eq!(o.text, "a = 4\n");
        // So is one made of another instance's or area's file.
        let r = reread(&o.path, None, o.format, None);
        assert!(!o.take_reread(&CheckOf { inst_id: "other".into(), ..o.check_of() }, r));
        let r = reread(&o.path, None, o.format, None);
        assert!(!o.take_reread(&CheckOf { area: Area::Shaders, ..o.check_of() }, r));
        assert_eq!(o.text, "a = 4\n");
        // With unsaved changes here, a change on disk is a conflict, not a reload.
        let mut o = open("a = 333\n");
        o.set_text("a = 5\n".into());
        std::fs::write(&path, "a = 6\n").unwrap();
        let r = reread(&o.path, o.stamp, o.format, None);
        assert!(o.take_reread(&o.check_of(), r));
        assert_eq!(o.text, "a = 5\n");
        assert_eq!(o.conflict.as_deref(), Some("a = 6\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_filter_by_path_or_mod() {
        let f = |p: &str| mc::ConfigFile { rel: p.into(), size: 1, modified: None };
        let groups = vec![Group { name: "Sodium".into(), icon: None, ids: vec!["sodium".into()], files: vec![f("sodium-options.json"), f("sodium-mixins.properties")] }, Group { name: "other".into(), icon: None, ids: Vec::new(), files: vec![f("mixins.json")] }];
        let all: Vec<PathBuf> = visible(&groups, "").into_iter().flat_map(|(_, f)| f).collect();
        assert_eq!(all.len(), 3);
        let sodium: Vec<PathBuf> = visible(&groups, "sodium").into_iter().flat_map(|(_, f)| f).collect();
        assert_eq!(sodium.len(), 2);
        let mixins: Vec<(usize, Vec<PathBuf>)> = visible(&groups, "MIXINS");
        assert_eq!(mixins.iter().map(|(n, f)| (*n, f.len())).collect::<Vec<_>>(), [(0, 1), (1, 1)]);
    }
}
