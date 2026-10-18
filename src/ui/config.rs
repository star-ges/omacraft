//! The config tab: an instance's mod configs, grouped by the mod they belong to, edited
//! as fields (TOML, JSON, JSON5) or as text. Saving writes back only what changed
//! (see `mod_config`), and never over a file the game rewrote meanwhile without asking.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    at: Instant,
}

#[derive(Default)]
pub struct ConfigState {
    listing: Option<Listing>,
    listing_p: Option<(String, Promise<Vec<Group>>)>,
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
    loading: Option<(String, PathBuf, Promise<mc::Loaded>)>,
    load_error: Option<(PathBuf, String)>,
    /// Save or text/fields, routed here by the app (save also works while typing).
    pub key: Option<Action>,
    checked: Option<Instant>,
    /// Long lists the user unfolded, by [`field_id`].
    unfolded: HashSet<String>,
}

/// The file being edited.
pub struct Open {
    pub inst_id: String,
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
}

fn field_id(f: &Field) -> String {
    format!("{}\u{1f}{}", f.section.join("\u{1f}"), f.key)
}

fn values(doc: &Result<Doc, mc::ParseError>) -> HashMap<String, Value> {
    doc.as_ref().map(|d| d.fields.iter().map(|f| (field_id(f), f.value.clone())).collect()).unwrap_or_default()
}

impl Open {
    fn new(inst_id: String, rel: PathBuf, loaded: mc::Loaded) -> Self {
        let format = Format::of(&rel).unwrap_or(Format::Text);
        let doc = mc::parse(format, &loaded.text);
        // Fields where there are some to show; text otherwise.
        let raw = !format.structured() || doc.as_ref().map_or(true, |d| d.fields.is_empty());
        Open {
            inst_id,
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
        }
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
        self.doc = mc::parse(self.format, &self.text);
    }

    /// Take the file as it is on disk now.
    fn reload(&mut self, text: String) {
        self.saved = text.clone();
        self.set_text(text);
        self.saved_values = values(&self.doc);
        self.stamp = mc::Stamp::of(&self.path);
        self.conflict = None;
    }

    fn name(&self) -> String {
        self.rel.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }
}

/// Somewhere to go once unsaved changes are dealt with.
pub enum Away {
    File(PathBuf),
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
    let s = &mut app.config;
    if s.open.as_ref().is_some_and(|o| o.dirty() && o.rel != rel) {
        app.modal = Some(Modal::ConfigLeave(Box::new(Leave { away: Away::File(rel) })));
        return;
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
    scroll_file: bool,
    edit: Option<(usize, Value)>,
    save: bool,
    revert: bool,
    raw: Option<bool>,
    take_disk: bool,
    keep_mine: bool,
    focus: Option<Zone>,
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
    let s = &mut app.config;

    // Another instance's file with nothing unsaved: let it go (unsaved ones wait in `guard`).
    if s.open.as_ref().is_some_and(|o| o.inst_id != inst.id && !o.dirty()) {
        s.open = None;
    }
    if s.listing.as_ref().is_some_and(|l| l.inst_id != inst.id) {
        s.listing = None;
        s.selected = None;
        s.file_filter.clear();
        s.field_filter.clear();
        s.field_sel = None;
    }

    // The file list, read in the background now and then.
    if let Some((id, p)) = &s.listing_p
        && let Some(r) = p.take()
    {
        if *id == inst.id {
            s.listing = Some(Listing { inst_id: id.clone(), groups: r.unwrap_or_default(), at: Instant::now() });
            // A file that couldn't be opened gets another try with each fresh listing.
            s.load_error = None;
        }
        s.listing_p = None;
    }
    let stale = s.listing.as_ref().is_none_or(|l| l.at.elapsed() > RELIST);
    if stale && s.listing_p.is_none() {
        let (i, cache) = (inst.clone(), s.mods.clone());
        let p = Promise::spawn(&rt, &ctx, async move {
            Ok(tokio::task::spawn_blocking(move || {
                let files = mc::list(&i);
                let mods = mc::known_mods(&i, &mut cache.lock().unwrap());
                mc::group(&files, &mods)
            })
            .await?)
        });
        s.listing_p = Some((inst.id.clone(), p));
        ctx.request_repaint_after(RELIST);
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
    if groups.is_empty() {
        w::empty_state(ui, &t, "\u{f107b}", "no config files yet", "Mods write their settings into config/ the first time the game starts with them. Play once, then come back here.");
        return;
    }

    let shown = visible(&groups, &s.file_filter);
    let flat: Vec<PathBuf> = shown.iter().flat_map(|(_, f)| f.iter().cloned()).collect();
    let all: Vec<&PathBuf> = groups.iter().flat_map(|g| g.files.iter().map(|f| &f.rel)).collect();
    // A file that's gone (and not being edited) drops its selection.
    if s.selected.as_ref().is_some_and(|sel| !all.contains(&sel) && s.open.as_ref().is_none_or(|o| o.rel != *sel || !o.dirty())) {
        s.selected = None;
    }
    if s.selected.is_none() {
        s.selected = flat.first().cloned();
    }
    let mut out = Out::default();

    // Keyboard on the file list.
    if let Some(n) = list_nav {
        let pos = s.selected.as_ref().and_then(|sel| flat.iter().position(|f| f == sel));
        match n {
            n if n.is_move() => {
                if let Some(p) = n.step(flat.len(), pos) {
                    out.pick = Some(flat[p].clone());
                    out.scroll_file = true;
                }
            }
            Nav::Activate | Nav::Edit => out.focus = Some(Zone::Side),
            Nav::Folder => super::open_path(&mc::config_dir(inst)),
            _ => {}
        }
    }

    // Load the selected file (in the background) unless it's the one open.
    if let Some((id, rel, p)) = &s.loading
        && let Some(r) = p.take()
    {
        let (id, rel) = (id.clone(), rel.clone());
        s.loading = None;
        if id == inst.id && s.selected.as_ref() == Some(&rel) {
            match r {
                Ok(loaded) => {
                    s.open = Some(Open::new(id, rel, loaded));
                    s.load_error = None;
                }
                Err(e) => s.load_error = Some((rel, format!("{e:#}"))),
            }
        }
    }
    if let Some(sel) = s.selected.clone() {
        let have = s.open.as_ref().is_some_and(|o| o.rel == sel && o.inst_id == inst.id);
        let pending = s.loading.as_ref().is_some_and(|(id, r, _)| *r == sel && *id == inst.id);
        let failed = s.load_error.as_ref().is_some_and(|(r, _)| *r == sel);
        if !have && !pending && !failed {
            let (i, r) = (inst.clone(), sel.clone());
            let p = Promise::spawn(&rt, &ctx, async move { tokio::task::spawn_blocking(move || mc::load(&i, &r)).await? });
            s.loading = Some((inst.id.clone(), sel, p));
        }
    }

    // The game (or anything else) may rewrite the open file.
    if let Some(o) = &mut s.open
        && s.checked.is_none_or(|c| c.elapsed() > RECHECK)
    {
        s.checked = Some(Instant::now());
        ctx.request_repaint_after(RECHECK);
        let now = mc::Stamp::of(&o.path);
        if now != o.stamp {
            o.stamp = now;
            if let Ok(disk) = mc::read_text(&o.path)
                && disk != o.saved
            {
                if o.dirty() {
                    o.conflict = Some(disk);
                } else {
                    o.reload(disk);
                    o.note = Some(("reloaded: it changed on disk".into(), Instant::now()));
                }
            }
        }
    }

    // Keys routed from the app.
    match key {
        Some(Action::SaveConfig) => out.save = true,
        Some(Action::RawConfig) => out.raw = s.open.as_ref().filter(|o| o.format.structured()).map(|o| !o.raw),
        _ => {}
    }

    let pane_w = (ui.available_width() * 0.3).clamp(220.0, 300.0);
    egui::Panel::left("config-files")
        .exact_size(pane_w)
        .resizable(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin { right: 14, ..Default::default() }))
        .show(ui, |ui| files_pane(ui, &t, s, &groups, &shown, list_nav == Some(Nav::Search), focus == Zone::Main, &mut out));

    let focused = focus == Zone::Side;
    match (&mut s.open, &s.load_error) {
        (Some(o), _) if s.selected.as_ref() == Some(&o.rel) && o.inst_id == inst.id => {
            let st = EditorState { field_filter: &mut s.field_filter, field_sel: &mut s.field_sel, scroll_field: &mut s.scroll_field, unfolded: &mut s.unfolded };
            editor(ui, &t, o, st, running, edit_nav, focused, &mut out);
        }
        (_, Some((rel, e))) if s.selected.as_ref() == Some(rel) => {
            w::empty_state(ui, &t, "\u{f05d6}", "can't open this file", e);
        }
        _ => {
            ui.add_space(20.0);
            ui.spinner();
        }
    }

    apply(app, out);
}

/// Carry out what the frame asked for.
fn apply(app: &mut App, out: Out) {
    if let Some(z) = out.focus {
        app.focus = z;
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
            o.saved_values = values(&mc::parse(o.format, &o.saved));
            o.stamp = mc::Stamp::of(&o.path);
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
fn files_pane(ui: &mut Ui, t: &Theme, s: &mut ConfigState, groups: &[Group], shown: &[(usize, Vec<PathBuf>)], focus_filter: bool, focused: bool, out: &mut Out) {
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
    egui::ScrollArea::vertical().id_salt("config-files").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        if shown.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("nothing matches").color(t.muted));
        }
        for (n, files) in shown {
            let g = &groups[*n];
            group_header(ui, t, &g.name, g.icon.as_deref(), files.len());
            for rel in files {
                let on = s.selected.as_ref() == Some(rel);
                // Under its mod's heading the file's own name is enough; the path is on hover.
                let label = rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let right = if open_dirty.as_ref() == Some(rel) { "●" } else { "" };
                let fmt = Format::of(rel).unwrap_or(Format::Text);
                let r = w::lead_row(ui, t, on, Lead::Glyph(glyph(fmt)), &label, right, None).on_hover_text(rel.to_string_lossy());
                if on && focused {
                    w::focus_mark(ui, t, r.rect);
                }
                if on && scroll {
                    r.scroll_to_me(None);
                }
                if r.clicked() {
                    out.pick = Some(rel.clone());
                    out.focus = Some(Zone::Main);
                }
                if r.double_clicked() {
                    out.focus = Some(Zone::Side);
                }
            }
            ui.add_space(6.0);
        }
    });
}

/// A mod's name over its files: icon, name, count and a hairline.
fn group_header(ui: &mut Ui, t: &Theme, name: &str, icon: Option<&Path>, count: usize) {
    let size = t.body_size() - 1.0;
    let h = size + 14.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let mut x = rect.left() + 6.0;
    let icon_size = size + 2.0;
    match icon {
        Some(p) => {
            let r = Rect::from_center_size(egui::pos2(x + icon_size / 2.0, rect.center().y), vec2(icon_size, icon_size));
            egui::Image::new(egui::ImageSource::Uri(format!("file://{}", p.display()).into())).corner_radius(2.0).paint_at(ui, r);
            x += icon_size + 8.0;
        }
        None => {
            let g = if name == mc::OTHER { "\u{f0224}" } else { "\u{f107b}" };
            ui.painter().text(egui::pos2(x, rect.center().y), egui::Align2::LEFT_CENTER, g, FontId::monospace(size), t.muted);
            x += icon_size + 8.0;
        }
    }
    let p = ui.painter();
    let c = p.layout_no_wrap(count.to_string(), FontId::monospace(size), t.muted);
    let cw = c.size().x;
    p.galley(egui::pos2(rect.right() - 10.0 - cw, rect.center().y - c.size().y / 2.0), c, t.muted);
    let clip = Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 20.0 - cw, rect.bottom()));
    let g = p.layout_no_wrap(name.to_string(), FontId::monospace(size), t.fg);
    let tw = g.size().x;
    p.with_clip_rect(clip).galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, t.fg);
    let (l, r) = (x + tw + 8.0, rect.right() - 18.0 - cw);
    if r > l {
        p.hline(l..=r, rect.center().y, Stroke::new(1.0, t.fg_a(0.10)));
    }
}

// -- the editor -------------------------------------------------------------------

struct EditorState<'a> {
    field_filter: &'a mut String,
    field_sel: &'a mut Option<String>,
    scroll_field: &'a mut bool,
    unfolded: &'a mut HashSet<String>,
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
    // Narrow: the name gets a line of its own over the buttons.
    let stacked = ui.available_width() < 560.0;
    if stacked {
        ui.horizontal(title);
        ui.add_space(4.0);
    }
    // Title and buttons; buttons first so a long name can't run under them.
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add_enabled_ui(dirty, |ui| w::primary_button(ui, t, "  save  ")).inner.on_hover_text("ctrl+s").clicked() {
                out.save = true;
            }
            if ui.add_enabled(dirty, egui::Button::new("revert")).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() || (key(Nav::Delete) && dirty && o.raw_mode()) {
                out.revert = true;
            }
            if structured {
                let mut raw = o.raw;
                ui.add_space(4.0);
                // Laid out right to left here: listed backwards, it reads "fields  text".
                if w::segmented(ui, t, &mut raw, &[(true, "text"), (false, "fields")]) {
                    out.raw = Some(raw);
                }
            }
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                super::open_path(o.path.parent().unwrap_or(&o.path));
            }
            if !stacked {
                ui.add_space(10.0);
                ui.with_layout(Layout::left_to_right(Align::Center), title);
            }
        });
    });

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
        let r = w::text_input(ui, st.field_filter, "/ filter settings", (ui.available_width() * 0.5).clamp(160.0, 320.0), key(Nav::Search));
        if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            st.field_filter.clear();
            r.surrender_focus();
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let n = shown.len();
            let total = doc.fields.len();
            let text = if n == total { format!("{total} settings") } else { format!("{n} of {total} settings") };
            ui.label(RichText::new(text).color(t.muted));
        });
    });
    ui.add_space(6.0);

    // Keyboard: move between fields, and act on the selected one.
    let ids: Vec<String> = shown.iter().map(|(_, f)| field_id(f)).collect();
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
    let focus_widget = key(Nav::Activate) || key(Nav::Edit);
    let wide = ui.available_width() >= 560.0;
    let key_w = (ui.available_width() * 0.5).clamp(180.0, 440.0);
    let scroll = std::mem::take(st.scroll_field);

    {
        egui::ScrollArea::vertical().id_salt(("config-fields", &o.rel)).auto_shrink(false).show(ui, |ui| {
            let mut section: Option<&[String]> = None;
            for (row, (n, f)) in shown.iter().enumerate() {
                if section != Some(f.section.as_slice()) {
                    section = Some(&f.section);
                    if !f.section.is_empty() {
                        ui.add_space(if row == 0 { 0.0 } else { 10.0 });
                        w::section(ui, t, &f.section.join("  ›  "));
                        if let Some(d) = doc.section_doc(&f.section) {
                            egui::Frame::new().inner_margin(egui::Margin { left: 2, bottom: 4, ..Default::default() }).show(ui, |ui| {
                                w::para(ui, RichText::new(d).color(t.muted).size(t.body_size() - 1.0));
                            });
                        }
                    }
                }
                let id = &ids[row];
                let selected = pos == Some(row);
                let changed = o.saved_values.get(id) != Some(&f.value);
                let ctx = Row { t, id, selected, focused: selected && focused, changed, wide, key_w };
                let kb_here = if selected { kb } else { None };
                let (rect, clicked, edit) = field_row(ui, &ctx, f, kb_here, selected && focus_widget, st.unfolded, out);
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
            ui.add_space(12.0);
        });
    }
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
                ui.add(egui::Label::new(RichText::new(&f.key).font(theme::bold(t.body_size())).color(color)).truncate()).on_hover_text(&f.key);
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
                ui.vertical(|ui| edit = value_part(ui, t, f, r.id, kb, focus_widget, unfolded, out));
            });
        } else {
            key_part(ui);
            ui.add_space(4.0);
            edit = value_part(ui, t, f, r.id, kb, focus_widget, unfolded, out);
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
fn value_part(ui: &mut Ui, t: &Theme, f: &Field, id: &str, kb: Option<Nav>, focus: bool, unfolded: &mut HashSet<String>, out: &mut Out) -> Option<Value> {
    let key = |n: Nav| kb == Some(n);
    let wid = egui::Id::new(("config-field", id));
    let mut edit = None;
    match &f.value {
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
                let text = format!("default {}", d.display());
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
                let label = ui.add(egui::Label::new(RichText::new(if *b { "true" } else { "false" }).color(if *b { t.fg } else { t.muted })).sense(Sense::click()));
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
        Scalar::Str(v) if !hint.allowed.is_empty() => {
            let mut options = hint.allowed.clone();
            if !options.contains(v) {
                options.insert(0, v.clone());
            }
            let mut pick = None;
            // Few short choices read best side by side; more go in a menu.
            let short = options.len() <= 4 && options.iter().map(|o| o.chars().count() + 3).sum::<usize>() as f32 * t.body_size() * 0.62 < ui.available_width();
            if short {
                let mut cur = v.clone();
                let opts: Vec<(String, &str)> = options.iter().map(|o| (o.clone(), o.as_str())).collect();
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
                egui::ComboBox::from_id_salt(wid).selected_text(v.as_str()).width(width).show_ui(ui, |ui| {
                    for o in &options {
                        if ui.selectable_label(o == v, o.as_str()).clicked() && o != v {
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
        Format::Toml | Format::Text => {
            let mut start = 0;
            for line in text.split_inclusive('\n') {
                let end = start + line.len();
                let trimmed = line.trim_start();
                let lead = start + (line.len() - trimmed.len());
                push(start..lead, t.fg);
                let comment_start = |c: u8| c == b'#' || (format == Format::Text && (c == b';' || c == b'!'));
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
        Away::File(rel) => {
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

    #[test]
    fn files_filter_by_path_or_mod() {
        let f = |p: &str| mc::ConfigFile { rel: p.into(), size: 1, modified: None };
        let groups = vec![Group { name: "Sodium".into(), icon: None, files: vec![f("sodium-options.json"), f("sodium-mixins.properties")] }, Group { name: "other".into(), icon: None, files: vec![f("mixins.json")] }];
        let all: Vec<PathBuf> = visible(&groups, "").into_iter().flat_map(|(_, f)| f).collect();
        assert_eq!(all.len(), 3);
        let sodium: Vec<PathBuf> = visible(&groups, "sodium").into_iter().flat_map(|(_, f)| f).collect();
        assert_eq!(sodium.len(), 2);
        let mixins: Vec<(usize, Vec<PathBuf>)> = visible(&groups, "MIXINS");
        assert_eq!(mixins.iter().map(|(n, f)| (*n, f.len())).collect::<Vec<_>>(), [(0, 1), (1, 1)]);
    }
}
