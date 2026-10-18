//! Mod sets and the search for the breaking mod, on screen: the sets bar at the top of
//! the mods tab, the sets dialog, the search's start dialog, the banner on the instance
//! while a search runs, and the question after each test game.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use egui::{Align, CornerRadius, Layout, RichText, Stroke, Ui, vec2};

use super::content::ModJobDone;
use super::nav::Nav;
use super::widgets::{self as w, Lead};
use super::{App, Confirm, ConfirmAction, Modal, ToastKind};
use crate::bisect::{self, Answer, Bisect, StepKind};
use crate::instance::Instance;
use crate::keymap::Action;
use crate::mod_sets::{self, Entry, ModSets};
use crate::task::{Promise, Task};
use crate::theme::Theme;

const SETS: &str = "\u{f0328}";
const SEARCH: &str = "\u{f0349}";
const FOUND: &str = "\u{f012c}";
const NOT_FOUND: &str = "\u{f02d7}";
const SAVE_INTO: &str = "save currently enabled mods to set";
const TICKED: &str = "\u{f0132}";
const UNTICKED: &str = "\u{f0131}";
const WARN: &str = "\u{f0026}";

/// Each instance's sets and search, read from disk and read again when their files
/// change (checked twice a second at most).
#[derive(Default)]
pub struct State {
    cache: HashMap<String, Cached>,
    /// Games that ended mid-search, to ask about once no other dialog is open:
    /// (instance id, what suggests the problem happened).
    asks: Vec<(String, Option<String>)>,
    /// Instances whose sets couldn't be saved when new mods joined: not tried again
    /// every frame.
    absorb_failed: std::collections::HashSet<String>,
}

struct Cached {
    checked: Instant,
    stamps: (Option<SystemTime>, Option<SystemTime>),
    sets: Result<ModSets, String>,
    bisect: Result<Option<Bisect>, String>,
}

fn stamp(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl State {
    fn get(&mut self, inst: &Instance) -> &Cached {
        let fresh = self.cache.get(&inst.id).is_some_and(|c| c.checked.elapsed() < Duration::from_millis(500));
        if !fresh {
            let stamps = (stamp(&mod_sets::path(inst)), stamp(&bisect::path(inst)));
            match self.cache.get_mut(&inst.id) {
                Some(c) if c.stamps == stamps => c.checked = Instant::now(),
                _ => {
                    let sets = mod_sets::load(inst).map_err(|e| format!("{e:#}"));
                    let bisect = bisect::load(inst).map_err(|e| format!("{e:#}"));
                    self.cache.insert(inst.id.clone(), Cached { checked: Instant::now(), stamps, sets, bisect });
                }
            }
        }
        &self.cache[&inst.id]
    }

    pub fn sets(&mut self, inst: &Instance) -> Result<ModSets, String> {
        self.get(inst).sets.clone()
    }

    pub fn bisect(&mut self, inst: &Instance) -> Option<Bisect> {
        self.get(inst).bisect.as_ref().ok().and_then(|b| b.clone())
    }

    /// Whether a search for the breaking mod has `inst`'s mods (running or showing its result).
    pub fn searching(&mut self, inst: &Instance) -> bool {
        self.get(inst).bisect.as_ref().is_ok_and(|b| b.is_some())
    }

    /// [`Self::searching`] as last read, for the hints bar (which can't read files).
    pub fn seen_searching(&self, id: &str) -> bool {
        self.cache.get(id).is_some_and(|c| c.bisect.as_ref().is_ok_and(|b| b.is_some()))
    }

    /// Read `inst`'s files again next time (after writing them).
    pub fn forget(&mut self, id: &str) {
        self.cache.remove(id);
    }
}

/// Who has an instance's mods right now: the one admission check every control that
/// switches mods goes through (the mods tab, sets, the crash panel, the search's own actions).
#[derive(Debug, Clone, Copy, Default)]
pub struct ModsBusy {
    pub starting: bool,
    pub running: bool,
    /// A mod job: a switch, update, rollback or a search's step or ending.
    pub jobs: bool,
    /// Anything installing into the instance.
    pub installing: bool,
    /// Whether a search for the breaking mod has them; None until its file has been read.
    pub search: Option<bool>,
}

impl ModsBusy {
    /// Why a mod can't be switched on or off by hand (crash panel), if it can't.
    pub fn by_hand(&self) -> Option<&'static str> {
        if self.starting {
            Some("the game is starting")
        } else if self.jobs {
            Some("mods are being switched or updated")
        } else if self.search != Some(false) {
            Some("the search for the breaking mod has them")
        } else {
            None
        }
    }

    /// Why the mods can't be switched to a set, or a search started, if they can't.
    pub fn switch(&self) -> Option<&'static str> {
        if self.starting {
            Some("the game is starting")
        } else if self.running {
            Some("the game is running")
        } else if self.jobs || self.installing {
            Some("mods are installing or updating")
        } else {
            self.by_hand()
        }
    }

    /// Why a search's own step (answer, stop, finish, try again) can't run now, if it can't:
    /// only one at a time, and never while a game has the mods.
    pub fn search_action(&self) -> Option<&'static str> {
        if self.starting {
            Some("the game is starting")
        } else if self.running {
            Some("the game is running")
        } else if self.jobs || self.installing {
            Some("mods are being switched, installed or updated")
        } else {
            None
        }
    }
}

/// [`ModsBusy`] for `inst` now.
pub fn busy(app: &mut App, inst: &Instance) -> ModsBusy {
    ModsBusy {
        starting: app.sessions.get(&inst.id).is_some_and(|s| s.starting()),
        running: app.is_running(&inst.id),
        jobs: app.content.updating(&inst.id),
        installing: app.content.busy_with(&inst.id),
        search: Some(app.mod_sets.searching(inst)),
    }
}

/// Why `inst`'s mods can't be switched to a set right now, if they can't.
fn switch_blocked(app: &mut App, inst: &Instance) -> Option<&'static str> {
    busy(app, inst).switch()
}

/// Run `f` on `inst`'s mods in the background as a mod job (so the instance counts as
/// busy, and launches wait), toasting what it returns.
fn job(app: &mut App, ctx: &egui::Context, inst: &Instance, stage: &str, f: impl FnOnce(&Instance) -> anyhow::Result<String> + Send + 'static) {
    let task = Task::new(format!("{stage} in {}", inst.name));
    task.stage(stage, 0);
    let (i, rt) = (inst.clone(), app.rt.clone());
    app.content.start_mod_job(&rt, ctx, &inst.id, None, task, async move {
        match tokio::task::spawn_blocking(move || f(&i)).await {
            Ok(Ok(text)) => ModJobDone::new(ToastKind::Info, text),
            Ok(Err(e)) => ModJobDone::new(ToastKind::Error, format!("{e:#}")),
            Err(e) => ModJobDone::new(ToastKind::Error, e.to_string()),
        }
    });
    app.mod_sets.forget(&inst.id);
}

/// Change `inst`'s sets on disk with `f`, toasting a failure.
fn edit_sets(app: &mut App, inst: &Instance, f: impl FnOnce(&mut ModSets) -> anyhow::Result<()>) -> bool {
    let r = mod_sets::load(inst).and_then(|mut sets| {
        f(&mut sets)?;
        mod_sets::save(inst, &sets)
    });
    app.mod_sets.forget(&inst.id);
    match r {
        Ok(()) => true,
        Err(e) => {
            app.toast(ToastKind::Error, format!("{e:#}"));
            false
        }
    }
}

/// A mod's title for messages.
fn title_of(names: &HashMap<String, String>, key: &str) -> String {
    names.get(key).cloned().unwrap_or_else(|| key.trim_start_matches("file:").to_string())
}

fn list_titles(names: &HashMap<String, String>, keys: &[String]) -> String {
    let mut t: Vec<String> = keys.iter().take(4).map(|k| title_of(names, k)).collect();
    if keys.len() > 4 {
        t.push(format!("{} more", keys.len() - 4));
    }
    t.join(", ")
}

// -- the bar in the mods tab ----------------------------------------------------

/// The mods tab's top line: the active set (and whether the mods still match it), the
/// sets dialog, and the search for the breaking mod. `found` is the mods with their
/// keys and titles, once read.
pub fn bar(app: &mut App, ui: &mut Ui, inst: &Instance, found: Option<(&[Entry], &HashMap<String, String>)>) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let empty = HashMap::new();
    let names = found.map_or(&empty, |(_, n)| n);
    // While a search runs its banner says so, and the mods are the search's until it ends.
    if app.mod_sets.bisect(inst).is_some() {
        return;
    }
    let sets = app.mod_sets.sets(inst);
    let busy = switch_blocked(app, inst);
    let sets = match sets {
        Ok(s) => s,
        Err(e) => {
            w::para(ui, RichText::new(format!("Couldn't read the mod sets: {e}")).color(t.red));
            return;
        }
    };
    // A mod switched on that the active set hasn't seen (just installed, say) joins it.
    if busy.is_none()
        && !app.mod_sets.absorb_failed.contains(&inst.id)
        && let Some((entries, _)) = found
        && sets.active().is_some_and(|s| entries.iter().any(|e| e.file.enabled && !s.seen(&e.key)))
        && !edit_sets(app, inst, |s| {
            s.absorb(entries);
            Ok(())
        })
    {
        app.mod_sets.absorb_failed.insert(inst.id.clone());
    }
    let modified = found.is_some_and(|(e, _)| sets.modified(e));
    let mut open = false;
    let (mut update, mut revert, mut find) = (false, false, false);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            find = w::button(ui, &format!("{SEARCH} find the breaking mod…")).on_hover_text("turn mods off half at a time to find the one causing a problem").clicked();
            open |= w::button(ui, "sets…").clicked();
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new(SETS).color(t.muted));
                match sets.active() {
                    Some(set) => {
                        let name = RichText::new(&set.name).font(crate::theme::bold(t.body_size())).color(t.fg_light);
                        open |= ui.add(egui::Label::new(name).sense(egui::Sense::click()).truncate()).on_hover_text("mod sets").on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                        if modified {
                            ui.label(RichText::new("·").color(t.muted));
                            ui.label(RichText::new("modified").color(t.yellow)).on_hover_text("the enabled mods differ from the set: update the set, or revert the mods");
                            if busy.is_none() {
                                update = w::button(ui, "update set").on_hover_text(format!("save the currently enabled mods to “{}”", set.name)).clicked();
                                revert = w::button(ui, "revert").on_hover_text(format!("switch the mods back to match “{}”", set.name)).clicked();
                            }
                        }
                    }
                    None if sets.sets.is_empty() => {
                        ui.add(egui::Label::new(RichText::new("no mod sets · save these mods as one to switch between arrangements").color(t.muted)).truncate());
                    }
                    None => {
                        ui.add(egui::Label::new(RichText::new(format!("no set chosen · {} saved", sets.sets.len())).color(t.muted)).truncate());
                    }
                }
            });
        });
    });

    // Mods the active set has never seen are off; say so once, with a way to add them.
    if let (Some(set), Some((entries, _))) = (sets.active(), found) {
        let unseen = set.unseen(entries);
        if !unseen.is_empty() && busy.is_none() {
            let (mut add, mut keep) = (false, false);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    keep = w::button(ui, "keep them off").clicked();
                    add = w::button(ui, "turn them on").clicked();
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let what = if unseen.len() == 1 { "1 mod is".to_string() } else { format!("{} mods are", unseen.len()) };
                        let text = format!("{what} new to “{}” and off: {}", set.name, list_titles(names, &unseen));
                        ui.add(egui::Label::new(RichText::new(text).color(t.muted)).truncate());
                    });
                });
            });
            if add {
                let paths: Vec<_> = entries.iter().filter(|e| unseen.contains(&e.key)).map(|e| e.file.path.clone()).collect();
                let mut failed = None;
                for p in paths {
                    if let Err(e) = mod_sets::rename(&p, true) {
                        failed = Some(format!("{e:#}"));
                    }
                }
                if let Some(e) = failed {
                    app.toast(ToastKind::Error, e);
                }
                // Now on, so they join the active set.
                let now: Vec<Entry> = entries
                    .iter()
                    .cloned()
                    .map(|mut e| {
                        e.file.enabled |= unseen.contains(&e.key);
                        e
                    })
                    .collect();
                edit_sets(app, inst, |s| {
                    s.absorb(&now);
                    Ok(())
                });
                app.content.invalidate();
            }
            if keep {
                edit_sets(app, inst, |s| {
                    s.keep_off(&unseen);
                    Ok(())
                });
            }
        }
    }

    if let Some(why) = busy.filter(|_| update || revert) {
        app.toast(ToastKind::Error, format!("Can't change the mods while {why}"));
    } else if update && let (Some(set), Some((entries, _))) = (sets.active(), found) {
        let name = set.name.clone();
        if edit_sets(app, inst, |s| s.update(&name, entries)) {
            app.toast(ToastKind::Info, format!("Saved the enabled mods to “{name}”"));
        }
    } else if revert && let Some(set) = sets.active() {
        switch(app, &ctx, inst, &set.name);
    }
    if open || app.content.key == Some(Action::ModSets) {
        app.content.key = None;
        open_sets(app, &ctx, inst, names.clone());
    }
    if find || app.content.key == Some(Action::FindBreaking) {
        app.content.key = None;
        open_bisect(app, &inst.id, Vec::new());
    }
}

/// Switch `inst`'s mods to set `name` in the background.
fn switch(app: &mut App, ctx: &egui::Context, inst: &Instance, name: &str) {
    if let Some(why) = switch_blocked(app, inst) {
        app.toast(ToastKind::Error, format!("Can't switch sets while {why}"));
        return;
    }
    let name = name.to_string();
    job(app, ctx, inst, "switching mods", move |i| {
        let c = mod_sets::switch(i, &name)?;
        Ok(if c.is_empty() { format!("The mods already match “{name}”") } else { format!("Switched to “{name}”: {}", c.summary()) })
    });
}

// -- the sets dialog -------------------------------------------------------------

pub struct SetsDialog {
    inst: Instance,
    /// The mods with their keys, read from the jars when the dialog opens.
    scan: Option<Promise<(Vec<Entry>, Needs)>>,
    entries: Option<Vec<Entry>>,
    /// Each mod's key → the installed mods it requires, so ticking one ticks those too.
    needs: Needs,
    names: HashMap<String, String>,
    selected: usize,
    new_name: String,
    rename: String,
    renaming: Option<String>,
    focus_new: bool,
    focus_rename: bool,
    /// The keyboard is on the selected set's mods (else on the sets).
    in_mods: bool,
    mod_sel: usize,
    filter: String,
    focus_filter: bool,
    /// What the last tick did besides itself ("also on: Fabric API, which Mod Menu needs").
    note: Option<String>,
}

type Needs = BTreeMap<String, BTreeSet<String>>;

/// Open the sets dialog for `inst`, with mod titles (by key) for its lists.
pub fn open_sets(app: &mut App, ctx: &egui::Context, inst: &Instance, names: HashMap<String, String>) {
    let i = inst.clone();
    let scan = Promise::spawn(&app.rt, ctx, async move {
        Ok(tokio::task::spawn_blocking(move || {
            let entries = mod_sets::scan(&i);
            let needs = mod_sets::needs(&entries);
            (entries, needs)
        })
        .await?)
    });
    let sets = app.mod_sets.sets(inst).unwrap_or_default();
    let selected = sets.sets.iter().position(|s| s.name == sets.active).unwrap_or(0);
    app.modal = Some(Modal::ModSets(Box::new(SetsDialog {
        inst: inst.clone(),
        scan: Some(scan),
        entries: None,
        needs: Needs::new(),
        names,
        selected,
        new_name: String::new(),
        rename: String::new(),
        renaming: None,
        focus_new: sets.sets.is_empty(),
        focus_rename: false,
        in_mods: false,
        mod_sel: 0,
        filter: String::new(),
        focus_filter: false,
        note: None,
    })));
}

/// The sets dialog. Returns true when it should close.
pub fn sets_dialog(app: &mut App, s: &mut SetsDialog, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    ui.set_width((screen.width() - 80.0).clamp(460.0, 820.0));
    if let Some(p) = &s.scan
        && let Some(r) = p.take()
    {
        let (entries, needs) = r.unwrap_or_default();
        s.entries = Some(entries);
        s.needs = needs;
        s.scan = None;
    }
    let inst = s.inst.clone();
    let sets = app.mod_sets.sets(&inst).unwrap_or_default();
    s.selected = s.selected.min(sets.sets.len().saturating_sub(1));
    let current = sets.sets.get(s.selected).cloned();
    if current.as_ref().map(|c| &c.name) != s.renaming.as_ref() {
        s.renaming = current.as_ref().map(|c| c.name.clone());
        s.rename = s.renaming.clone().unwrap_or_default();
        s.note = None;
    }
    // The selected set's checklist: every installed mod once (by key), the filter applied.
    let rows: Vec<ModRow> = match (&current, &s.entries) {
        (Some(set), Some(entries)) => mod_rows(set, entries, &s.names, &s.needs, &s.filter),
        _ => Vec::new(),
    };

    w::heading(ui, &t, "mod sets");
    ui.add_space(4.0);
    w::para(ui, RichText::new(format!("Named arrangements of {}'s mods: tick a set's mods on the right. Switching to a set turns mods on and off; nothing is removed.", inst.name)).color(t.muted));
    ui.add_space(10.0);

    let kb = app.modal_nav();
    let key = |n: Nav| kb == Some(n);
    match kb {
        Some(Nav::Right) if current.is_some() => s.in_mods = true,
        Some(Nav::Left) => s.in_mods = false,
        _ => {}
    }
    s.mod_sel = s.mod_sel.min(rows.len().saturating_sub(1));
    let moved = kb.filter(|n| n.is_move());
    if s.in_mods {
        if let Some(n) = moved.and_then(|n| n.step(rows.len(), Some(s.mod_sel))) {
            s.mod_sel = n;
        }
    } else if let Some(n) = moved.and_then(|n| n.step(sets.sets.len(), Some(s.selected))) {
        s.selected = n;
    }
    let mut tick = if s.in_mods && key(Nav::Toggle) { rows.get(s.mod_sel).map(|r| (r.key.clone(), !r.on)) } else { None };
    s.focus_filter |= key(Nav::Search) && current.is_some();
    s.focus_new |= key(Nav::Add);
    s.focus_rename |= key(Nav::Rename) && current.is_some();
    let blocked = switch_blocked(app, &inst);
    let mut close = false;
    let (mut do_switch, mut do_update, mut do_delete) = (key(Nav::Activate), key(Nav::Copy), key(Nav::Delete));
    let height = (screen.height() - 300.0).clamp(220.0, 420.0);

    ui.columns(2, |cols| {
        // -- left: the sets, and a new one from the mods as they are
        let ui = &mut cols[0];
        w::section(ui, &t, "sets");
        egui::ScrollArea::vertical().id_salt("mod-sets").max_height(height - 50.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if sets.sets.is_empty() {
                w::para(ui, RichText::new("none yet: name the mods as they are now below").color(t.muted));
            }
            for (n, set) in sets.sets.iter().enumerate() {
                let on = n == s.selected;
                let mut right = format!("{} on", set.on.len());
                if set.name == sets.active {
                    right = format!("active · {right}");
                }
                let r = w::lead_row(ui, &t, on, Lead::Glyph(SETS), &set.name, &right, None);
                if on && !s.in_mods {
                    w::focus_mark(ui, &t, r.rect);
                }
                if r.clicked() {
                    s.selected = n;
                    s.in_mods = false;
                }
                if r.double_clicked() {
                    do_switch = true;
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let r = w::text_input(ui, &mut s.new_name, "new set from the mods as they are", ui.available_width() - 60.0, std::mem::take(&mut s.focus_new));
            let ok = !s.new_name.trim().is_empty() && s.entries.is_some();
            let add = ui.add_enabled_ui(ok, |ui| w::button(ui, "add")).inner.clicked() || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if add && let Some(entries) = &s.entries {
                let name = s.new_name.trim().to_string();
                if edit_sets(app, &inst, |x| x.add(&name, entries)) {
                    s.new_name.clear();
                    s.selected = sets.sets.len();
                }
            }
        });

        // -- right: the selected set
        let ui = &mut cols[1];
        let Some(set) = &current else {
            w::empty_state(ui, &t, SETS, "save your first set", "Name a set below. A new set starts with the currently enabled mods; tick or untick mods to change the set.");
            return;
        };
        w::section(ui, &t, "name");
        ui.horizontal(|ui| {
            let r = w::text_input(ui, &mut s.rename, "", ui.available_width() - 80.0, std::mem::take(&mut s.focus_rename));
            let new = s.rename.trim().to_string();
            let ok = !new.is_empty() && new != set.name;
            let go = ui.add_enabled_ui(ok, |ui| w::button(ui, "rename")).inner.clicked() || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go && edit_sets(app, &inst, |x| x.rename(&set.name, &new)) {
                s.renaming = Some(new);
            }
        });
        let mut all: Option<bool> = None;
        w::section(ui, &t, "mods in set");
        ui.horizontal(|ui| {
            if let Some(entries) = &s.entries {
                let keys: BTreeSet<&String> = entries.iter().map(|e| &e.key).collect();
                let on = keys.iter().filter(|k| set.on.contains(**k)).count();
                ui.label(RichText::new(format!("{on} of {} on", keys.len())).color(t.muted).size(t.body_size() - 1.0));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, "none").on_hover_text("untick the mods shown").clicked() {
                    all = Some(false);
                }
                if w::button(ui, "all").on_hover_text("tick the mods shown").clicked() {
                    all = Some(true);
                }
            });
        });
        if s.entries.as_ref().is_some_and(|e| e.len() > 8) || !s.filter.is_empty() {
            w::text_input(ui, &mut s.filter, "filter", ui.available_width() - 16.0, std::mem::take(&mut s.focus_filter));
            ui.add_space(4.0);
        }
        match &s.entries {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("reading the mods…").color(t.muted));
                });
            }
            Some(_) => {
                egui::ScrollArea::vertical().id_salt("mod-set-mods").max_height(height - 150.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if rows.is_empty() {
                        w::para(ui, RichText::new(if s.filter.is_empty() { "no mods installed" } else { "no mods match the filter" }).color(t.muted));
                    }
                    for (n, r) in rows.iter().enumerate() {
                        let sel = s.in_mods && n == s.mod_sel;
                        let (glyph, mark) = match (r.on, r.needed_by.is_empty()) {
                            (true, _) => (TICKED, t.accent),
                            (false, false) => (WARN, t.yellow),
                            (false, true) => (UNTICKED, t.muted),
                        };
                        let (right, color) = if !r.needed_by.is_empty() {
                            (needed_label(&r.needed_by), t.yellow)
                        } else if r.on != r.now_on {
                            let what = if r.on { "turns on" } else { "turns off" };
                            (if r.seen { what.to_string() } else { format!("new · {what}") }, if r.on { t.green } else { t.muted })
                        } else if !r.seen {
                            ("new".to_string(), t.muted)
                        } else {
                            (String::new(), t.muted)
                        };
                        let row = w::check_row(ui, &t, sel, r.on, glyph, mark, &r.title, &right, color);
                        if sel {
                            w::focus_mark(ui, &t, row.rect);
                            if moved.is_some() {
                                row.scroll_to_me(None);
                            }
                        }
                        if row.clicked() {
                            s.in_mods = true;
                            s.mod_sel = n;
                            tick = Some((r.key.clone(), !r.on));
                        }
                    }
                });
            }
        }
        if let Some(note) = &s.note {
            ui.add_space(4.0);
            w::para(ui, RichText::new(note).color(t.muted).size(t.body_size() - 1.0));
        }
        if let Some(on) = all {
            let keys: Vec<String> = rows.iter().map(|r| r.key.clone()).collect();
            let keys: Vec<String> = if on { mod_sets::with_needs(&keys, &s.needs).into_iter().collect() } else { keys };
            if edit_sets(app, &inst, |x| x.set_mods(&set.name, &keys, on)) {
                s.note = None;
            }
        }
        if let Some((k, on)) = tick.take() {
            // Ticking a mod ticks what it needs; unticking one that others need only warns (on its row).
            let keys: Vec<String> = if on { mod_sets::with_needs(std::slice::from_ref(&k), &s.needs).into_iter().filter(|x| *x == k || !set.on.contains(x)).collect() } else { vec![k.clone()] };
            if edit_sets(app, &inst, |x| x.set_mods(&set.name, &keys, on)) {
                let also: Vec<String> = keys.iter().filter(|x| **x != k).cloned().collect();
                s.note = (!also.is_empty()).then(|| format!("Also ticked {}, which {} needs.", list_titles(&s.names, &also), title_of(&s.names, &k)));
            }
        }

        // What switching to the set (or, for the active one, applying it) would do; nothing when it matches.
        ui.add_space(8.0);
        let c = s.entries.as_ref().map(|e| set.change(e));
        let active = set.name == sets.active;
        if let Some(c) = c.as_ref().filter(|c| !c.is_empty()) {
            w::para(ui, RichText::new(format!("set {}", c.summary())).color(t.fg_light));
            ui.add_space(6.0);
        }
        if let Some(why) = blocked {
            w::para(ui, RichText::new(format!("Can't switch while {why}.")).color(t.yellow));
            ui.add_space(4.0);
        }
        let changes = c.as_ref().is_some_and(|c| !c.is_empty());
        ui.horizontal_wrapped(|ui| {
            if changes {
                do_switch |= ui.add_enabled_ui(blocked.is_none(), |ui| w::primary_button(ui, &t, if active { "apply" } else { "switch" })).inner.clicked();
            }
            do_update |= ui.add_enabled_ui(s.entries.is_some() && changes, |ui| w::button(ui, SAVE_INTO)).inner.clicked();
            do_delete |= w::danger_button(ui, &t, "delete").clicked();
        });
    });

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let k = |a| app.keymap.label(a);
        w::key_hint(ui, &t, &k(Action::Activate), "switch");
        w::key_hint(ui, &t, &format!("{}/{}", k(Action::FocusLeft), k(Action::FocusRight)), "sets/mods");
        if s.in_mods {
            w::key_hint(ui, &t, &k(Action::Toggle), "tick");
        }
        w::key_hint(ui, &t, &k(Action::Add), "new");
        w::key_hint(ui, &t, &k(Action::Rename), "rename");
        w::key_hint(ui, &t, &k(Action::Copy), SAVE_INTO);
        w::key_hint(ui, &t, &k(Action::Delete), "delete");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| close = w::button(ui, "close").clicked());
    });

    let Some(set) = current else { return close };
    if do_switch {
        if let Some(why) = blocked {
            app.toast(ToastKind::Error, format!("Can't switch sets while {why}"));
        } else if s.entries.is_some() {
            switch(app, &ctx, &inst, &set.name);
            return true;
        }
    }
    if do_update && let Some(entries) = &s.entries {
        if let Some(why) = blocked.filter(|_| set.name == sets.active) {
            app.toast(ToastKind::Error, format!("Can't change the active set while {why}"));
        } else if edit_sets(app, &inst, |x| x.update(&set.name, entries)) {
            app.toast(ToastKind::Info, format!("Saved the enabled mods to “{}”", set.name));
        }
    }
    if do_delete {
        app.modal = Some(Modal::Confirm(Confirm {
            title: "delete mod set".into(),
            body: format!("Delete the set “{}”? The mods stay as they are; only its name and list go.", set.name),
            button: "delete".into(),
            action: ConfirmAction::DeleteModSet(inst.id.clone(), set.name.clone()),
        }));
        return true;
    }
    close
}

/// One mod in the sets dialog's checklist.
struct ModRow {
    key: String,
    title: String,
    /// Ticked in the set.
    on: bool,
    /// Enabled in the mods folder now.
    now_on: bool,
    seen: bool,
    /// Ticked mods that require this one, when it isn't ticked.
    needed_by: Vec<String>,
}

/// "needed by Mod Menu", "needed by Mod Menu, Jade", "needed by Mod Menu + 5 more".
fn needed_label(by: &[String]) -> String {
    match by {
        [a] => format!("needed by {a}"),
        [a, b] => format!("needed by {a}, {b}"),
        [a, rest @ ..] => format!("needed by {a} + {} more", rest.len()),
        [] => String::new(),
    }
}

fn mod_rows(set: &mod_sets::ModSet, entries: &[Entry], names: &HashMap<String, String>, needs: &Needs, filter: &str) -> Vec<ModRow> {
    let mut by_key: BTreeMap<&str, bool> = BTreeMap::new();
    for e in entries {
        *by_key.entry(&e.key).or_default() |= e.file.enabled;
    }
    let filter = filter.trim().to_lowercase();
    let mut rows: Vec<ModRow> = by_key
        .into_iter()
        .map(|(k, now_on)| {
            let on = set.on.contains(k);
            let needed_by = if on { Vec::new() } else { set.on.iter().filter(|o| needs.get(*o).is_some_and(|n| n.contains(k))).map(|o| title_of(names, o)).collect() };
            ModRow { key: k.to_string(), title: title_of(names, k), on, now_on, seen: set.seen(k), needed_by }
        })
        .filter(|r| filter.is_empty() || r.title.to_lowercase().contains(&filter) || r.key.contains(&filter))
        .collect();
    rows.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    rows
}

/// Delete set `name` of instance `id` (confirmed already).
pub fn delete_set(app: &mut App, id: &str, name: &str) {
    let Some(inst) = app.instances.iter().find(|i| i.id == id).cloned() else { return };
    if edit_sets(app, &inst, |s| {
        s.remove(name);
        Ok(())
    }) {
        app.toast(ToastKind::Info, format!("Deleted the set “{name}”"));
    }
}

// -- the search for the breaking mod --------------------------------------------

pub enum BisectDialog {
    Start(StartDialog),
    Ask(AskDialog),
}

pub struct StartDialog {
    inst: Instance,
    rows: Vec<Row>,
    selected: usize,
}

struct Row {
    file: String,
    title: String,
    mark: Mark,
    /// Named by whoever opened the dialog (a crash report): tried first.
    first: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Test,
    Fine,
    Keep,
}

impl Mark {
    fn next(self) -> Mark {
        match self {
            Mark::Test => Mark::Fine,
            Mark::Fine => Mark::Keep,
            Mark::Keep => Mark::Test,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mark::Test => "test",
            Mark::Fine => "known fine",
            Mark::Keep => "keep on",
        }
    }
}

pub struct AskDialog {
    inst_id: String,
    /// What suggests the problem happened (a crash), if anything.
    why: Option<String>,
    choice: usize,
}

/// Start finding the breaking mod in instance `id`: opens the start dialog, where every
/// enabled mod is to be tested unless marked fine or kept on. `first` names files (as
/// enabled, e.g. `sodium-0.6.0.jar`) to try first, such as a crash report's suspects;
/// they're listed at the top. If a search is already running, asks about its step instead.
pub fn open_bisect(app: &mut App, id: &str, first: Vec<String>) {
    let Some(inst) = app.instances.iter().find(|i| i.id == id).cloned() else { return };
    if inst.loader.modrinth().is_none() {
        app.toast(ToastKind::Error, "Vanilla instances don't load mods");
        return;
    }
    if let Some(b) = app.mod_sets.bisect(&inst) {
        if b.done.is_none() {
            app.modal = Some(Modal::Bisect(Box::new(BisectDialog::Ask(AskDialog { inst_id: inst.id.clone(), why: None, choice: 0 }))));
        }
        return;
    }
    let first: Vec<String> = first.iter().map(|f| bisect::base(f)).collect();
    let mut rows: Vec<Row> = inst
        .mods()
        .into_iter()
        .filter(|f| f.enabled && !f.is_dir)
        .map(|f| Row { title: app.content.title(&f).unwrap_or_else(|| f.display_name().to_string()), first: first.contains(&f.name), file: f.name, mark: Mark::Test })
        .collect();
    rows.sort_by(|a, b| b.first.cmp(&a.first).then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())));
    app.modal = Some(Modal::Bisect(Box::new(BisectDialog::Start(StartDialog { inst, rows, selected: 0 }))));
}

/// The search's dialogs. Returns true when it should close.
pub fn bisect_dialog(app: &mut App, d: &mut BisectDialog, ui: &mut Ui) -> bool {
    match d {
        BisectDialog::Start(s) => start_dialog(app, s, ui),
        BisectDialog::Ask(a) => ask_dialog(app, a, ui),
    }
}

/// About how many steps a search over `n` suspects takes (halving, then one to confirm).
fn steps_for(n: usize) -> usize {
    if n <= 1 { 1 } else { (usize::BITS - (n - 1).leading_zeros()) as usize + 1 }
}

fn worlds_note(ui: &mut Ui, t: &Theme) {
    w::para(ui, RichText::new("Test from the title screen or a new world: a world opened with mods missing can lose what those mods added. Omacraft backs a world up first when that might happen.").color(t.yellow));
}

fn start_dialog(app: &mut App, s: &mut StartDialog, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    ui.set_width((screen.width() - 80.0).clamp(460.0, 680.0));
    let kb = app.modal_nav();
    if let Some(n) = kb.filter(|n| n.is_move()).and_then(|n| n.step(s.rows.len(), Some(s.selected))) {
        s.selected = n;
    }
    if kb == Some(Nav::Toggle)
        && let Some(r) = s.rows.get_mut(s.selected)
    {
        r.mark = r.mark.next();
    }
    let testing = s.rows.iter().filter(|r| r.mark == Mark::Test).count();

    w::heading(ui, &t, "find the breaking mod");
    ui.add_space(6.0);
    w::para(ui, "Omacraft turns about half the mods off at a time. You play, then say whether the problem occurred, and the suspects halve each time.");
    ui.add_space(4.0);
    let about = if testing == 0 { "Mark at least one mod to test.".to_string() } else { format!("{testing} mods to test: about {} games. Your mods go back exactly as they are now when the search ends.", steps_for(testing)) };
    w::para(ui, RichText::new(about).color(t.muted));
    ui.add_space(4.0);
    worlds_note(ui, &t);
    ui.add_space(8.0);
    w::section(ui, &t, "the enabled mods");
    let height = (screen.height() - 360.0).clamp(160.0, 380.0);
    egui::ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if s.rows.is_empty() {
            w::para(ui, RichText::new("No mods are enabled.").color(t.muted));
        }
        for (n, r) in s.rows.iter_mut().enumerate() {
            let on = n == s.selected;
            let right = if r.first && r.mark == Mark::Test { "suspect · test".to_string() } else { r.mark.label().to_string() };
            let glyph = match r.mark {
                Mark::Test => SEARCH,
                Mark::Fine => FOUND,
                Mark::Keep => "\u{f0403}",
            };
            let row = w::lead_row(ui, &t, on, Lead::Glyph(glyph), &r.title, &right, None);
            if on {
                w::focus_mark(ui, &t, row.rect);
                if kb.is_some_and(|k| k.is_move()) {
                    row.scroll_to_me(None);
                }
            }
            if row.clicked() {
                if on {
                    r.mark = r.mark.next();
                }
                s.selected = n;
            }
        }
    });
    ui.add_space(4.0);
    w::para(ui, RichText::new(format!("{} or a click on the selected mod marks the mod known fine (off unless needed) or keep on (on in every step).", app.keymap.label(Action::Toggle))).color(t.muted).size(t.body_size() - 1.0));
    ui.add_space(12.0);
    let mut go = kb == Some(Nav::Activate);
    let mut close = false;
    ui.horizontal(|ui| {
        go |= ui.add_enabled_ui(testing > 0, |ui| w::primary_button(ui, &t, "start")).inner.clicked();
        close = w::button(ui, "cancel").clicked();
    });
    if go && testing > 0 {
        let inst = s.inst.clone();
        if let Some(why) = switch_blocked(app, &inst) {
            app.toast(ToastKind::Error, format!("Can't start while {why}"));
            return false;
        }
        let of = |m: Mark| s.rows.iter().filter(|r| r.mark == m).map(|r| r.file.clone()).collect::<Vec<_>>();
        let opts = bisect::Start { keep: of(Mark::Keep), good: of(Mark::Fine), first: s.rows.iter().filter(|r| r.first).map(|r| r.file.clone()).collect() };
        job(app, &ctx, &inst, "finding the breaking mod", move |i| {
            let b = bisect::start(i, &opts)?;
            let on = b.step.as_ref().map_or(0, |s| s.on.len());
            Ok(format!("Step 1: {on} of {} mods on. Play, then say whether the problem happened.", b.original.values().filter(|o| **o).count()))
        });
        app.inst_tab = super::instances::InstTab::Mods;
        return true;
    }
    close
}

const ANSWERS: [(Answer, &str, &str); 3] = [
    (Answer::Broken, "problem occurred", "the problem showed up, or the game crashed"),
    (Answer::Works, "no problem occurred", "the game ran fine"),
    (Answer::Skip, "can't tell", "try a different half of the mods instead"),
];

fn ask_dialog(app: &mut App, a: &mut AskDialog, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    ui.set_width(520.0);
    let Some(inst) = app.instances.iter().find(|i| i.id == a.inst_id).cloned() else { return true };
    let Some(b) = app.mod_sets.bisect(&inst).filter(|b| b.done.is_none()) else { return true };
    let kb = app.modal_nav();
    if let Some(n) = kb.filter(|n| n.is_move()).and_then(|n| n.step(ANSWERS.len(), Some(a.choice))) {
        a.choice = n;
    }
    let step = b.step.clone();
    w::heading(ui, &t, "did the problem occur?");
    ui.add_space(6.0);
    let (n, of) = b.progress();
    let on = step.as_ref().map_or(0, |s| s.on.len());
    let kind = if step.as_ref().is_some_and(|s| s.kind == StepKind::Confirm) { " · checking the last suspect" } else { "" };
    ui.label(RichText::new(format!("step {n} of ~{of} · {} suspects · {on} mods on{kind}", b.suspects.len())).color(t.muted));
    ui.add_space(8.0);
    if let Some(why) = &a.why {
        w::para(ui, RichText::new(why).color(t.yellow));
        ui.add_space(6.0);
    }
    let mut pick = kb == Some(Nav::Activate);
    for (n, (_, label, hint)) in ANSWERS.iter().enumerate() {
        let on = n == a.choice;
        let r = w::lead_row(ui, &t, on, Lead::None, label, hint, None);
        if on {
            w::focus_mark(ui, &t, r.rect);
        }
        // A click only selects: nothing is sent until "answer", so a slip can be corrected.
        if r.clicked() {
            a.choice = n;
        }
    }
    ui.add_space(12.0);
    let mut close = false;
    ui.horizontal(|ui| {
        pick |= w::primary_button(ui, &t, "answer").clicked();
        close = w::button(ui, "close").on_hover_text("answer later from the banner on the instance").clicked();
    });
    if pick {
        answer(app, &ctx, &inst, ANSWERS[a.choice].0);
        return true;
    }
    close
}

/// Send an answer for `inst`'s current step, then switch to the next.
fn answer(app: &mut App, ctx: &egui::Context, inst: &Instance, a: Answer) {
    if let Some(why) = busy(app, inst).search_action() {
        app.toast(ToastKind::Error, format!("Can't answer while {why}"));
        return;
    }
    let (i, rt) = (inst.clone(), app.rt.clone());
    let task = Task::new(format!("finding the breaking mod in {}", inst.name));
    task.stage("switching mods", 0);
    app.content.start_mod_job(&rt, ctx, &inst.id, None, task, async move {
        let r = tokio::task::spawn_blocking({
            let i = i.clone();
            move || bisect::answer(&i, a)
        })
        .await;
        let b = match r {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return ModJobDone::new(ToastKind::Error, format!("{e:#}")),
            Err(e) => return ModJobDone::new(ToastKind::Error, e.to_string()),
        };
        let Some(done) = &b.done else {
            let (n, of) = b.progress();
            let on = b.step.as_ref().map_or(0, |s| s.on.len());
            return ModJobDone::new(ToastKind::Info, format!("Step {n} of ~{of}: {on} mods on, {} suspects. Play again.", b.suspects.len()));
        };
        // Its Modrinth page, if it has one, for "open its page".
        if !done.culprits.is_empty() {
            let dir = i.game_dir().join("mods");
            let files: Vec<(String, Option<String>)> = done.culprits.iter().map(|c| (c.file.clone(), crate::net::sha1_file(&dir.join(&c.file)))).collect();
            let hashes: Vec<String> = files.iter().filter_map(|(_, h)| h.clone()).collect();
            if let Ok(found) = crate::content_meta::lookup(hashes).await {
                let _ = tokio::task::spawn_blocking(move || {
                    bisect::edit(&i, |b| {
                        for c in b.done.iter_mut().flat_map(|d| d.culprits.iter_mut()) {
                            let hash = files.iter().find(|(f, _)| *f == c.file).and_then(|(_, h)| h.as_ref());
                            if let Some(p) = hash.and_then(|h| found.get(h)) {
                                c.project = Some(p.id.clone());
                                c.name = c.name.take().or(Some(p.title.clone()));
                            }
                        }
                    })
                })
                .await;
            }
        }
        match done.culprits.as_slice() {
            [] => ModJobDone::new(ToastKind::Info, "Couldn't narrow the problem to one mod; the mods tab says why".into()),
            [c] => ModJobDone::new(ToastKind::Info, format!("Breaking mod found: {}", c.label())),
            many => ModJobDone::new(ToastKind::Info, format!("The breaking mod is one of {} mods that need each other", many.len())),
        }
    });
    app.mod_sets.forget(&inst.id);
}

/// A game in `id` ended: if it was a step of a search, ask how it went. `exit` is its
/// exit code, or why it has none.
pub fn game_ended(app: &mut App, id: &str, exit: &Result<i32, String>) {
    let Some(inst) = app.instances.iter().find(|i| i.id == id).cloned() else { return };
    let Some(step) = app.mod_sets.bisect(&inst).filter(|b| b.done.is_none()).and_then(|b| b.step) else { return };
    let why = match exit {
        _ if bisect::crashed_since(&inst, step.began) => Some("The game wrote a crash report, so the problem most likely occurred.".to_string()),
        Ok(0) => None,
        Ok(c) => Some(format!("The game exited with code {c}, so the problem most likely occurred.")),
        Err(e) if e == "killed by signal" => None,
        Err(e) => Some(format!("The game {e}.")),
    };
    app.mod_sets.asks.retain(|(i, _)| i != id);
    app.mod_sets.asks.push((id.to_string(), why));
    show_ask(app);
}

/// Ask about the oldest ended game, if no other dialog is open.
fn show_ask(app: &mut App) {
    if app.modal.is_some() || app.mod_sets.asks.is_empty() {
        return;
    }
    let (inst_id, why) = app.mod_sets.asks.remove(0);
    let choice = if why.is_some() { 0 } else { 1 };
    app.modal = Some(Modal::Bisect(Box::new(BisectDialog::Ask(AskDialog { inst_id, why, choice }))));
}

/// The banner on an instance while a search for its breaking mod runs, or shows its result.
pub fn banner(app: &mut App, ui: &mut Ui, inst: &Instance) {
    show_ask(app);
    let Some(b) = app.mod_sets.bisect(inst) else { return };
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let blocked = busy(app, inst).search_action();
    let running = app.is_running(&inst.id);
    let busy = blocked.is_some();
    ui.add_space(12.0);
    let color = match &b.done {
        Some(d) if d.culprits.is_empty() => t.yellow,
        Some(_) => t.green,
        None => t.accent,
    };
    let mut act: Option<Act> = None;
    let r = egui::Frame::new()
        .fill(Theme::alpha(color, 0.06))
        .stroke(Stroke::new(1.0, Theme::alpha(color, 0.35)))
        .corner_radius(CornerRadius::ZERO)
        .inner_margin(egui::Margin { left: 16, right: 12, top: 10, bottom: 10 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            // A switch that didn't finish, or an ending that couldn't find every mod, comes first.
            if !b.unresolved.is_empty() {
                let what = list_titles(&HashMap::new(), &b.unresolved);
                w::para(ui, RichText::new(format!("Couldn't find {what} in the mods folder, so the search is kept to remember how the mods were. Put the files back and try again, or forget the search and sort them out by hand.")).color(t.red));
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled_ui(!running && !busy, |ui| {
                        if w::button(ui, "try again").clicked() {
                            act = Some(Act::Recover);
                        }
                        if w::button(ui, "forget the search").on_hover_text("leave the mods as they are now").clicked() {
                            act = Some(Act::Forget);
                        }
                    });
                });
                return;
            }
            if !b.applied {
                w::para(ui, RichText::new("The mods didn't all switch. Nothing is lost: the search knows how they should be.").color(t.red));
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled_ui(!running && !busy, |ui| w::button(ui, "try again")).inner.clicked() {
                        act = Some(Act::Recover);
                    }
                    if b.ending.is_none() && ui.add_enabled_ui(!running && !busy, |ui| w::button(ui, "stop and put my mods back")).inner.clicked() {
                        act = Some(Act::Restore);
                    }
                });
                return;
            }
            match &b.done {
                None => {
                    let (n, of) = b.progress();
                    let on = b.step.as_ref().map_or(0, |s| s.on.len());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(SEARCH).color(color));
                        ui.label(RichText::new("finding the breaking mod").font(crate::theme::bold(t.body_size())).color(t.fg_light));
                        let confirm = b.step.as_ref().is_some_and(|s| s.kind == StepKind::Confirm);
                        let what = if confirm { "checking the last suspect".to_string() } else { format!("{} suspects left", b.suspects.len()) };
                        ui.add(egui::Label::new(RichText::new(format!("step {n} of ~{of}  ·  {what}  ·  {on} of {} mods on", b.original.values().filter(|o| **o).count())).color(t.muted)).truncate());
                    });
                    let say = if running { "Playing. Close the game once you've seen whether the problem occurs." } else { "Play, then say whether the problem occurred. Test from the title screen or a new world: worlds opened with mods missing can lose what those mods added." };
                    w::para(ui, RichText::new(say).color(t.fg));
                    // Answering goes through the dialog, where a choice can be changed before it's sent.
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled_ui(!running && !busy, |ui| w::button(ui, "answer…")).inner.clicked() {
                            act = Some(Act::Ask);
                        }
                        if ui.add_enabled_ui(!busy, |ui| w::button(ui, "stop and put my mods back")).inner.clicked() {
                            act = Some(Act::Restore);
                        }
                        ui.add_space(8.0);
                        w::key_hint(ui, &t, &app.keymap.label(Action::FindBreaking), "answer");
                    });
                }
                Some(done) => {
                    ui.horizontal(|ui| {
                        let (glyph, title) = match done.culprits.as_slice() {
                            [] => (NOT_FOUND, "couldn't narrow the problem to one mod".to_string()),
                            [c] => (FOUND, format!("breaking mod found: {}", c.label())),
                            many => (FOUND, format!("breaking mod is one of: {}", many.iter().map(|c| c.label()).collect::<Vec<_>>().join(", "))),
                        };
                        ui.label(RichText::new(glyph).color(color));
                        ui.add(egui::Label::new(RichText::new(title).font(crate::theme::bold(t.body_size())).color(t.fg_light)).truncate());
                    });
                    let mut note = done.note.clone();
                    if let [c] = done.culprits.as_slice() {
                        note = format!("{note} ({})", c.file);
                    }
                    w::para(ui, RichText::new(note).color(t.fg));
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!running && !busy, |ui| {
                            if done.culprits.is_empty() {
                                if w::button(ui, "put my mods back").clicked() {
                                    act = Some(Act::Restore);
                                }
                            } else {
                                let what = if done.culprits.len() == 1 { "mod" } else { "mods" };
                                let culprits = done.culprits.iter().map(|c| c.file.clone()).collect();
                                let label = match b.dependents(&culprits).len() {
                                    0 => format!("disable {what}"),
                                    1 => format!("disable {what} + 1 dependent"),
                                    n => format!("disable {what} + {n} dependents"),
                                };
                                if w::button(ui, &label).on_hover_text("put the mods back as they were, but with the breaking mod and its dependents off").clicked() {
                                    act = Some(Act::Disable);
                                }
                                if w::button(ui, "put my mods back").on_hover_text("every mod back as before the search, the breaking mod included").clicked() {
                                    act = Some(Act::Restore);
                                }
                            }
                        });
                        for c in done.culprits.iter().filter(|c| c.project.is_some()).take(1) {
                            if w::button(ui, "modrinth page").clicked() {
                                super::open_url(&format!("https://modrinth.com/project/{}", c.project.as_deref().unwrap_or_default()));
                            }
                        }
                    });
                }
            }
        });
    // The accent edge, like a focus mark.
    let rect = r.response.rect;
    ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height())), CornerRadius::ZERO, color);

    // Checked again as it runs: a job may have started since the buttons were drawn.
    if let Some(why) = blocked.filter(|_| act.as_ref().is_some_and(|a| !matches!(a, Act::Ask | Act::Forget))) {
        app.toast(ToastKind::Error, format!("Can't do that while {why}"));
        return;
    }
    match act {
        Some(Act::Ask) => open_bisect(app, &inst.id, Vec::new()),
        Some(Act::Restore) => job(app, &ctx, inst, "putting the mods back", |i| {
            bisect::restore(i)?;
            Ok("Put the mods back as they were".into())
        }),
        Some(Act::Recover) => job(app, &ctx, inst, "switching mods", |i| {
            bisect::recover(i)?;
            Ok(match bisect::load(i)? {
                Some(b) if !b.unresolved.is_empty() => format!("Still couldn't find {}", b.unresolved.join(", ")),
                Some(_) => "Switched the mods for the search".into(),
                None => "Put the mods back as they were".into(),
            })
        }),
        Some(Act::Forget) => {
            app.modal = Some(Modal::Confirm(Confirm {
                title: "forget the search".into(),
                body: "Forget the search for the breaking mod? The mods stay as they are now, and the ones it couldn't find aren't put back.".into(),
                button: "forget".into(),
                action: ConfirmAction::ForgetSearch(inst.id.clone()),
            }));
        }
        Some(Act::Disable) => job(app, &ctx, inst, "putting the mods back", |i| {
            let also = bisect::disable_culprits(i)?;
            Ok(match also.len() {
                0 => "Put the mods back with the breaking mod off".into(),
                n => format!("Put the mods back with the breaking mod off, plus {n} that depend on the breaking mod: {}", also.join(", ")),
            })
        }),
        None => {}
    }
}

enum Act {
    Ask,
    Restore,
    Disable,
    Recover,
    Forget,
}

/// Forget instance `id`'s search (confirmed already), leaving its mods as they are.
pub fn forget_search(app: &mut App, ctx: &egui::Context, id: &str) {
    let Some(inst) = app.instances.iter().find(|i| i.id == id).cloned() else { return };
    if let Some(why) = busy(app, &inst).search_action() {
        app.toast(ToastKind::Error, format!("Can't forget the search while {why}"));
        return;
    }
    job(app, ctx, &inst, "forgetting the search", |i| {
        bisect::forget(i)?;
        Ok("Forgot the search; the mods stay as they are".into())
    });
}

/// The `b` key: answer the running search's step, or start one from the mods tab.
pub fn key(app: &mut App, inst_id: &str) {
    let Some(inst) = app.instances.iter().find(|i| i.id == inst_id).cloned() else { return };
    match app.mod_sets.bisect(&inst) {
        Some(b) if b.done.is_none() => open_bisect(app, inst_id, Vec::new()),
        Some(_) => {}
        None if app.inst_tab == super::instances::InstTab::Mods => app.content.key = Some(Action::FindBreaking),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_estimates() {
        assert_eq!([1, 2, 3, 4, 5, 8, 9, 14, 100].map(steps_for), [1, 2, 3, 3, 4, 4, 5, 5, 8]);
    }

    #[test]
    fn search_controls_share_one_admission_check() {
        let idle = ModsBusy { search: Some(false), ..Default::default() };
        assert!(idle.by_hand().is_none() && idle.switch().is_none() && idle.search_action().is_none());
        // The crash panel's toggles: never while a search has the mods, or before that's known.
        let searching = ModsBusy { search: Some(true), ..idle };
        assert!(searching.by_hand().is_some() && searching.switch().is_some());
        assert!(searching.search_action().is_none(), "the search's own actions still run");
        assert!(ModsBusy { search: None, ..idle }.by_hand().is_some());
        // Stop, answer or try again while a step is being switched: refused.
        let switching = ModsBusy { jobs: true, ..searching };
        assert!(switching.search_action().is_some() && switching.by_hand().is_some());
        for b in [ModsBusy { running: true, ..searching }, ModsBusy { starting: true, ..searching }, ModsBusy { installing: true, ..searching }] {
            assert!(b.search_action().is_some(), "{b:?}");
        }
        assert!(ModsBusy { starting: true, ..idle }.by_hand().is_some());
    }

    #[test]
    fn marks_cycle() {
        assert!(Mark::Test.next() == Mark::Fine && Mark::Fine.next() == Mark::Keep && Mark::Keep.next() == Mark::Test);
    }
}
