//! Mod sets and the search for the breaking mod, on screen: the sets bar at the top of
//! the mods tab, the sets dialog, the search's start dialog, the banner on the instance
//! while a search runs, and the question after each test game.

use std::collections::HashMap;
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
const SAVE_INTO: &str = "save current mods into it";
const SETS_MATCH: &str = "the mods match it";

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

/// Why `inst`'s mods can't be switched to a set right now, if they can't.
fn switch_blocked(app: &mut App, inst: &Instance) -> Option<&'static str> {
    if app.sessions.get(&inst.id).is_some_and(|s| s.starting()) {
        Some("the game is starting")
    } else if app.is_running(&inst.id) {
        Some("the game is running")
    } else if app.content.busy_with(&inst.id) {
        Some("mods are installing or updating")
    } else if app.mod_sets.searching(inst) {
        Some("the search for the breaking mod has them")
    } else {
        None
    }
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
                            ui.label(RichText::new("modified").color(t.yellow)).on_hover_text("the mods differ from the set: update it, or switch them back");
                            if busy.is_none() {
                                update = w::button(ui, "update set").on_hover_text(format!("save the mods as they are now into “{}”", set.name)).clicked();
                                revert = w::button(ui, "revert").on_hover_text(format!("switch the mods back to “{}”", set.name)).clicked();
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
            app.toast(ToastKind::Info, format!("Saved the mods into “{name}”"));
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
    scan: Option<Promise<Vec<Entry>>>,
    entries: Option<Vec<Entry>>,
    names: HashMap<String, String>,
    selected: usize,
    new_name: String,
    rename: String,
    renaming: Option<String>,
    focus_new: bool,
    focus_rename: bool,
}

/// Open the sets dialog for `inst`, with mod titles (by key) for its lists.
pub fn open_sets(app: &mut App, ctx: &egui::Context, inst: &Instance, names: HashMap<String, String>) {
    let i = inst.clone();
    let scan = Promise::spawn(&app.rt, ctx, async move { Ok(tokio::task::spawn_blocking(move || mod_sets::scan(&i)).await?) });
    let sets = app.mod_sets.sets(inst).unwrap_or_default();
    let selected = sets.sets.iter().position(|s| s.name == sets.active).unwrap_or(0);
    app.modal = Some(Modal::ModSets(Box::new(SetsDialog {
        inst: inst.clone(),
        scan: Some(scan),
        entries: None,
        names,
        selected,
        new_name: String::new(),
        rename: String::new(),
        renaming: None,
        focus_new: sets.sets.is_empty(),
        focus_rename: false,
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
        s.entries = Some(r.unwrap_or_default());
        s.scan = None;
    }
    let inst = s.inst.clone();
    let sets = app.mod_sets.sets(&inst).unwrap_or_default();
    s.selected = s.selected.min(sets.sets.len().saturating_sub(1));
    let current = sets.sets.get(s.selected).cloned();
    if current.as_ref().map(|c| &c.name) != s.renaming.as_ref() {
        s.renaming = current.as_ref().map(|c| c.name.clone());
        s.rename = s.renaming.clone().unwrap_or_default();
    }

    w::heading(ui, &t, "mod sets");
    ui.add_space(4.0);
    w::para(ui, RichText::new(format!("Named arrangements of {}'s mods. Switching one turns mods on and off; nothing is removed.", inst.name)).color(t.muted));
    ui.add_space(10.0);

    let kb = app.modal_nav();
    let key = |n: Nav| kb == Some(n);
    if let Some(n) = kb.filter(|n| n.is_move()).and_then(|n| n.step(sets.sets.len(), Some(s.selected))) {
        s.selected = n;
    }
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
                if on {
                    w::focus_mark(ui, &t, r.rect);
                }
                if r.clicked() {
                    s.selected = n;
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
            w::empty_state(ui, &t, SETS, "save your first set", "Give the mods as they are now a name, then change them and save another.");
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
        w::section(ui, &t, if set.name == sets.active { "the mods now" } else { "switching to it" });
        match &s.entries {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("reading the mods…").color(t.muted));
                });
            }
            Some(entries) => {
                let c = set.change(entries);
                let title_of_path = |p: &std::path::Path| {
                    entries.iter().find(|e| e.file.path == p).map(|e| title_of(&s.names, &e.key)).unwrap_or_default()
                };
                if c.is_empty() {
                    ui.label(RichText::new(SETS_MATCH).color(t.muted));
                } else {
                    ui.label(RichText::new(c.summary()).color(t.fg_light));
                }
                egui::ScrollArea::vertical().id_salt("mod-set-change").max_height(height - 150.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let mut lines: Vec<(bool, String)> = c.enable.iter().map(|p| (true, title_of_path(p))).chain(c.disable.iter().map(|p| (false, title_of_path(p)))).collect();
                    lines.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
                    for (on, name) in lines {
                        ui.horizontal(|ui| {
                            let (mark, color) = if on { ("+", t.green) } else { ("−", t.muted) };
                            ui.label(RichText::new(mark).color(color));
                            ui.add(egui::Label::new(RichText::new(name).color(if on { t.fg } else { t.muted })).truncate());
                        });
                    }
                });
                if !c.unseen.is_empty() {
                    let n = c.unseen.len();
                    let text = format!("{} new to it {} off: {}", if n == 1 { "1 mod".to_string() } else { format!("{n} mods") }, if n == 1 { "stays" } else { "stay" }, list_titles(&s.names, &c.unseen));
                    w::para(ui, RichText::new(text).color(t.muted));
                }
            }
        }
        ui.add_space(10.0);
        if let Some(why) = blocked {
            w::para(ui, RichText::new(format!("Can't switch while {why}.")).color(t.yellow));
            ui.add_space(4.0);
        }
        let changes = s.entries.as_ref().is_some_and(|e| !set.change(e).is_empty());
        ui.horizontal(|ui| {
            if changes {
                do_switch |= ui.add_enabled_ui(blocked.is_none(), |ui| w::primary_button(ui, &t, "switch")).inner.clicked();
            }
            do_update |= ui.add_enabled_ui(s.entries.is_some() && changes, |ui| w::button(ui, SAVE_INTO)).inner.clicked();
            do_delete |= w::danger_button(ui, &t, "delete").clicked();
        });
    });

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let k = |a| app.keymap.label(a);
        w::key_hint(ui, &t, &k(Action::Activate), "switch");
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
            app.toast(ToastKind::Info, format!("Saved the mods into “{}”", set.name));
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
    w::para(ui, "Omacraft turns about half the mods off at a time. You play, then say whether the problem happened, and the suspects halve each time.");
    ui.add_space(4.0);
    let about = if testing == 0 { "Mark at least one mod to test.".to_string() } else { format!("{testing} mods to test: about {} games. Your mods go back exactly as they are now when it ends.", steps_for(testing)) };
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
    w::para(ui, RichText::new(format!("{} or a click on the selected mod marks it known fine (off unless needed) or keep on (on in every step).", app.keymap.label(Action::Toggle))).color(t.muted).size(t.body_size() - 1.0));
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
    (Answer::Broken, "it happened", "the problem showed up (or the game crashed)"),
    (Answer::Works, "it didn't", "the game ran fine"),
    (Answer::Skip, "can't tell", "try a different half instead"),
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
    w::heading(ui, &t, "did the problem happen?");
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
        if r.clicked() {
            a.choice = n;
            pick = true;
        }
    }
    ui.add_space(12.0);
    let mut close = false;
    ui.horizontal(|ui| {
        pick |= w::primary_button(ui, &t, "answer").clicked();
        close = w::button(ui, "not yet").on_hover_text("answer later from the instance").clicked();
    });
    if pick {
        answer(app, &ctx, &inst, ANSWERS[a.choice].0);
        return true;
    }
    close
}

/// Send an answer for `inst`'s current step, then switch to the next.
fn answer(app: &mut App, ctx: &egui::Context, inst: &Instance, a: Answer) {
    if app.is_running(&inst.id) {
        app.toast(ToastKind::Error, "Close the game first, then answer");
        return;
    }
    if app.content.busy_with(&inst.id) {
        app.toast(ToastKind::Error, "Wait for the mods to finish switching");
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
            if let Ok(found) = crate::content_meta::lookup(hashes).await
                && let Ok(Some(mut b)) = bisect::load(&i)
            {
                for c in b.done.iter_mut().flat_map(|d| d.culprits.iter_mut()) {
                    let hash = files.iter().find(|(f, _)| *f == c.file).and_then(|(_, h)| h.as_ref());
                    if let Some(p) = hash.and_then(|h| found.get(h)) {
                        c.project = Some(p.id.clone());
                        c.name = c.name.take().or(Some(p.title.clone()));
                    }
                }
                let _ = bisect::save(&i, &b);
            }
        }
        match done.culprits.as_slice() {
            [] => ModJobDone::new(ToastKind::Info, "Couldn't pin it on one mod; see the instance for why".into()),
            [c] => ModJobDone::new(ToastKind::Info, format!("Found it: {}", c.label())),
            many => ModJobDone::new(ToastKind::Info, format!("Found it among {} mods that need each other", many.len())),
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
        _ if bisect::crashed_since(&inst, step.began) => Some("The game wrote a crash report, so it most likely happened.".to_string()),
        Ok(0) => None,
        Ok(c) => Some(format!("The game exited with code {c}, so it most likely happened.")),
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
    let running = app.is_running(&inst.id);
    let busy = app.content.busy_with(&inst.id);
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
                    let say = if running { "Playing. Close the game when you've seen whether the problem happens." } else { "Play, then say whether the problem happened. Test from the title screen or a new world: worlds opened with mods missing can lose what they added." };
                    w::para(ui, RichText::new(say).color(t.fg));
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::button(ui, "stop and put my mods back").clicked() {
                                act = Some(Act::Restore);
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.add_enabled_ui(!running && !busy, |ui| {
                                    for (a, label, _) in ANSWERS {
                                        if w::button(ui, label).clicked() {
                                            act = Some(Act::Answer(a));
                                        }
                                    }
                                });
                                ui.add_space(8.0);
                                w::key_hint(ui, &t, &app.keymap.label(Action::FindBreaking), "answer");
                            });
                        });
                    });
                }
                Some(done) => {
                    ui.horizontal(|ui| {
                        let (glyph, title) = match done.culprits.as_slice() {
                            [] => (NOT_FOUND, "couldn't pin it on one mod".to_string()),
                            [c] => (FOUND, format!("found it: {}", c.label())),
                            many => (FOUND, format!("found it: one of {}", many.iter().map(|c| c.label()).collect::<Vec<_>>().join(", "))),
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
                                let it = if done.culprits.len() == 1 { "it" } else { "them" };
                                let culprits = done.culprits.iter().map(|c| c.file.clone()).collect();
                                let label = match b.dependents(&culprits).len() {
                                    0 => format!("turn {it} off"),
                                    n => format!("turn {it} off (and {n} that need {it})"),
                                };
                                if w::button(ui, &label).on_hover_text("put the mods back as they were, with this one and whatever needs it off").clicked() {
                                    act = Some(Act::Disable);
                                }
                                if w::button(ui, &format!("keep {it}, put my mods back")).clicked() {
                                    act = Some(Act::Restore);
                                }
                            }
                        });
                        for c in done.culprits.iter().filter(|c| c.project.is_some()).take(1) {
                            if w::button(ui, "open its page").clicked() {
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

    match act {
        Some(Act::Answer(a)) => answer(app, &ctx, inst, a),
        Some(Act::Restore) if running => app.toast(ToastKind::Error, "Close the game first"),
        Some(Act::Restore) => job(app, &ctx, inst, "putting the mods back", |i| {
            bisect::restore(i)?;
            Ok("Put the mods back as they were".into())
        }),
        Some(Act::Disable) => job(app, &ctx, inst, "putting the mods back", |i| {
            let also = bisect::disable_culprits(i)?;
            Ok(match also.len() {
                0 => "Put the mods back, with the breaking one off".into(),
                n => format!("Put the mods back, with the breaking one off, and {n} that need it: {}", also.join(", ")),
            })
        }),
        None => {}
    }
}

enum Act {
    Answer(Answer),
    Restore,
    Disable,
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
    fn marks_cycle() {
        assert!(Mark::Test.next() == Mark::Fine && Mark::Fine.next() == Mark::Keep && Mark::Keep.next() == Mark::Test);
    }
}
