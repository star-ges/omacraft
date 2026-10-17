//! Singleplayer worlds in `saves/` on the left, the selected world's backups on the right.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{RichText, Ui};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, Confirm, ConfirmAction, Modal, ToastKind};
use crate::backups::{self, Owner, Snapshot};
use crate::instance::Instance;
use crate::launch::QuickPlay;
use crate::saves::{self, World};
use crate::task::{Promise, Task};

/// An instance's worlds and their backups, read together in the background.
#[derive(Clone, Default)]
pub struct Listing {
    worlds: Vec<World>,
    /// World folder → its snapshots, newest first. Includes worlds that are gone.
    backups: BTreeMap<String, Vec<Snapshot>>,
}

impl Listing {
    fn read(saves: PathBuf, owner: Owner) -> Self {
        backups::recover_worlds(owner).into_iter().for_each(|e| eprintln!("{e}"));
        let mut worlds = saves::list_worlds(&saves);
        for w in &mut worlds {
            w.played_in = crate::world_sets::read_record(&w.dir).map(|r| (r.instance, r.instance_name));
        }
        Listing { worlds, backups: backups::all_worlds(owner) }
    }

    /// World folders with backups but no world any more.
    fn gone(&self) -> Vec<String> {
        self.backups.keys().filter(|f| !self.worlds.iter().any(|w| &w.folder == *f)).cloned().collect()
    }
}

/// What a background world job leaves behind: a folder to select, and a note.
pub struct JobDone {
    pub select: Option<String>,
    pub note: Option<String>,
}

#[derive(Default)]
pub struct WorldsState {
    /// Instance id and its listing, kept on screen while a refresh runs.
    list: Option<(String, Listing)>,
    /// When `list` was read; None once it's out of date.
    fetched: Option<Instant>,
    /// Background read of an instance's `saves/` and backups.
    loading: Option<(String, Promise<Listing>)>,
    /// Duplicates, backups and restores: (task, result, instance id).
    pub jobs: Vec<(Task, Promise<JobDone>, String)>,
    selected: Option<String>,
    /// The selected backup, by folder.
    backup: Option<PathBuf>,
    /// World dir being renamed, and the new name.
    rename: Option<(PathBuf, String)>,
}

impl WorldsState {
    pub fn invalidate(&mut self) {
        self.fetched = None;
        // A read already under way may predate the change.
        self.loading = None;
    }

    /// The instance's worlds and backups, re-read in the background every few seconds.
    /// None until the first read for this instance lands.
    fn sync(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context, inst: &Instance) -> Option<&Listing> {
        let inst_id = inst.id.as_str();
        if let Some((id, p)) = &self.loading
            && let Some(r) = p.take()
        {
            if id == inst_id {
                self.list = Some((id.clone(), r.unwrap_or_default()));
                self.fetched = Some(Instant::now());
            }
            self.loading = None;
        }
        // Selection and a pending rename belong to the instance they were made in.
        if self.list.as_ref().is_some_and(|(id, _)| id != inst_id) {
            self.list = None;
            self.fetched = None;
            self.selected = None;
            self.backup = None;
            self.rename = None;
        }
        if self.loading.as_ref().is_some_and(|(id, _)| id != inst_id) {
            self.loading = None;
        }
        let fresh = self.fetched.is_some_and(|at| at.elapsed() < Duration::from_secs(3));
        if !fresh && self.loading.is_none() {
            let (saves, i) = (inst.saves_dir(), inst.clone());
            let p = Promise::spawn(rt, ctx, async move { Ok(tokio::task::spawn_blocking(move || Listing::read(saves, Owner::of(&i))).await?) });
            self.loading = Some((inst_id.to_string(), p));
        }
        self.list.as_ref().map(|(_, l)| l)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.jobs.iter().map(|(t, ..)| t)
    }

    pub fn busy_with(&self, inst_id: &str) -> bool {
        self.jobs.iter().any(|(.., id)| id == inst_id)
    }

    /// A finished job selects its world if its instance is still on screen.
    pub fn finished(&mut self, inst_id: &str, select: Option<String>) {
        if let Some(folder) = select
            && self.list.as_ref().is_some_and(|(id, _)| id == inst_id)
        {
            self.selected = Some(folder);
        }
        self.invalidate();
    }

    fn start(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context, inst_id: &str, label: &str, stage: String, job: impl FnOnce() -> anyhow::Result<JobDone> + Send + 'static) {
        let task = Task::new(label);
        task.stage(stage, 0);
        let p = Promise::spawn(rt, ctx, async move { tokio::task::spawn_blocking(job).await? });
        self.jobs.push((task, p, inst_id.to_string()));
    }
}

/// Why `inst`'s worlds can't be backed up or restored right now, if they can't.
fn worlds_busy(app: &App, inst: &Instance) -> Option<String> {
    if app.worlds.busy_with(&inst.id) {
        return Some("another world job is running".into());
    }
    if app.sharing.is_some() {
        return Some("shared folders are moving".into());
    }
    let running = std::iter::once(inst).chain(crate::world_sets::others(&inst.world_set, &inst.id, &app.instances)).find(|i| app.is_running(&i.id));
    running.map(|i| if i.id == inst.id { "it's running".to_string() } else { format!("{} is running in the same world set", i.name) })
}

/// Restore a backup (confirmed already) in the background.
pub fn start_restore(app: &mut App, ctx: &egui::Context, id: &str, snap: Snapshot) {
    let Some(inst) = app.instances.iter().find(|i| i.id == id).cloned() else { return };
    if let Some(why) = worlds_busy(app, &inst) {
        app.toast(ToastKind::Error, format!("Can't restore while {why}"));
        return;
    }
    let folder = snap.dir.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let rt = app.rt.clone();
    app.worlds.start(&rt, ctx, id, "restore", format!("Restoring {folder}"), move || {
        backups::restore_world(&snap, &inst.saves_dir())?;
        Ok(JobDone { select: Some(folder.clone()), note: Some(format!("Restored {folder} from {}", snap.when())) })
    });
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let Some(listing) = app.worlds.sync(&app.rt, ui.ctx(), inst).cloned() else {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| ui.label(RichText::new("reading worlds…").color(t.muted)));
        return;
    };
    let gone = listing.gone();
    if listing.worlds.is_empty() && gone.is_empty() {
        w::empty_state(ui, &t, "󰕒", "no worlds yet", "Create one in game, or drop a world folder into saves.");
        ui.vertical_centered(|ui| {
            if w::button(ui, "open saves folder").clicked() {
                super::open_path(&inst.saves_dir());
            }
        });
        return;
    }
    // Worlds, then the ones that only have backups left. Selection is by folder.
    let folders: Vec<String> = listing.worlds.iter().map(|w| w.folder.clone()).chain(gone.iter().cloned()).collect();
    let typing = app.worlds.rename.is_some();
    let main_kb = if typing { None } else { app.nav_for(Zone::Main) };
    let side_kb = if typing { None } else { app.nav_for(Zone::Side) };
    let zone = app.focus;
    let s = &mut app.worlds;
    let pos = s.selected.as_ref().and_then(|f| folders.iter().position(|x| x == f));
    let mut scroll = false;
    if let Some(p) = main_kb.filter(|n| n.is_move()).and_then(|n| n.step(folders.len(), pos)) {
        s.selected = Some(folders[p].clone());
        s.backup = None;
        scroll = true;
    }
    let sel_folder = s.selected.clone().filter(|f| folders.contains(f));
    let sel = sel_folder.as_ref().and_then(|f| listing.worlds.iter().find(|w| &w.folder == f)).cloned();
    let snaps: Vec<Snapshot> = sel_folder.as_ref().and_then(|f| listing.backups.get(f)).cloned().unwrap_or_default();

    let mut out = Out::default();
    ui.columns(2, |cols| {
        world_list(&mut cols[0], app, inst, &listing, &gone, sel.as_ref(), sel_folder.as_deref(), main_kb, zone == Zone::Main, scroll, &mut out);
        backup_list(&mut cols[1], app, inst, sel.as_ref(), sel_folder.as_deref(), &snaps, side_kb, zone == Zone::Side, &mut out);
    });
    if out.to_main {
        app.focus = Zone::Main;
    }
    if out.to_side {
        app.focus = Zone::Side;
    }
    if let Some(folder) = out.play {
        app.launch(ui.ctx(), &inst.id, Some(QuickPlay::World(folder)));
    }
}

#[derive(Default)]
struct Out {
    play: Option<String>,
    to_main: bool,
    to_side: bool,
}

#[allow(clippy::too_many_arguments)]
fn world_list(
    ui: &mut Ui,
    app: &mut App,
    inst: &Instance,
    listing: &Listing,
    gone: &[String],
    sel: Option<&World>,
    sel_folder: Option<&str>,
    kb: Option<Nav>,
    focused: bool,
    scroll: bool,
    out: &mut Out,
) {
    let t = app.t.clone();
    let key = |n: Nav| kb == Some(n);
    let moving = app.sharing.is_some();
    ui.horizontal_wrapped(|ui| {
        if let Some(wd) = sel {
            if w::primary_button(ui, &t, "󰐊 play").clicked() || key(Nav::Activate) {
                out.play = Some(wd.folder.clone());
            }
            if w::button(ui, "rename").clicked() || key(Nav::Rename) {
                app.worlds.rename = Some((wd.dir.clone(), wd.name.clone()));
            }
            // One at a time per instance, so two copies can't pick the same folder name.
            // Not while shared folders move: the copy could land in a folder on its way out.
            if (w::button(ui, "duplicate").clicked() || key(Nav::Duplicate)) && !app.worlds.busy_with(&inst.id) && !moving {
                let world = wd.clone();
                let rt = app.rt.clone();
                app.worlds.start(&rt, ui.ctx(), &inst.id, "duplicate", format!("Duplicating {}", wd.name), move || {
                    Ok(JobDone { select: Some(saves::duplicate_world(&world)?), note: None })
                });
            }
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                super::open_path(&wd.dir);
            }
            if w::danger_button(ui, &t, "delete…").clicked() || key(Nav::Delete) {
                let kept = if listing.backups.contains_key(&wd.folder) { " Its backups are kept." } else { "" };
                app.modal = Some(Modal::Confirm(Confirm {
                    title: "delete world".into(),
                    body: format!("Permanently delete “{}”?{kept}", wd.name),
                    button: "delete".into(),
                    action: ConfirmAction::DeletePath(wd.dir.clone()),
                }));
            }
        } else if sel_folder.is_some() {
            ui.label(RichText::new("deleted; restore it from a backup").color(t.muted));
        } else {
            ui.label(RichText::new("select a world").color(t.muted));
        }
        if w::button(ui, "saves folder").clicked() {
            super::open_path(&inst.saves_dir());
        }
    });
    if !inst.world_set.is_empty() {
        ui.add(egui::Label::new(RichText::new(format!("\u{f0ac} world set “{}”", inst.world_set)).color(t.muted)).truncate())
            .on_hover_text("these worlds are shared with the other instances in the set");
    }

    if let Some((dir, name)) = &mut app.worlds.rename {
        ui.add_space(6.0);
        let mut done = None;
        ui.horizontal(|ui| {
            w::field_label(ui, &t, "new name");
            let r = w::text_input(ui, name, "", 220.0, true);
            if w::primary_button(ui, &t, "save").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                done = Some(true);
            }
            if w::button(ui, "cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                done = Some(false);
            }
        });
        match done {
            Some(true) => {
                if let Err(e) = saves::rename_world(dir, name.trim()) {
                    app.toast(ToastKind::Error, format!("Couldn't rename: {e:#}"));
                }
                app.worlds.rename = None;
                app.worlds.invalidate();
            }
            Some(false) => app.worlds.rename = None,
            None => {}
        }
    }
    ui.add_space(6.0);

    let s = &mut app.worlds;
    egui::ScrollArea::vertical().id_salt("worlds").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let mut row = |ui: &mut Ui, folder: &str, r: egui::Response, play: bool| {
            let on = sel_folder == Some(folder);
            if on && focused {
                w::focus_mark(ui, &t, r.rect);
            }
            if on && scroll {
                r.scroll_to_me(None);
            }
            if r.clicked() {
                s.selected = Some(folder.to_string());
                s.backup = None;
                out.to_main = true;
            }
            if play && r.double_clicked() {
                out.play = Some(folder.to_string());
            }
        };
        for wd in &listing.worlds {
            let mode = if wd.hardcore { "hardcore" } else { wd.mode };
            let mut sub = vec![mode.to_string()];
            if let Some(v) = &wd.version {
                sub.push(v.clone());
            }
            sub.push(w::ago(wd.last_played));
            if wd.folder != wd.name {
                sub.push(wd.folder.clone());
            }
            if let Some((id, name)) = &wd.played_in
                && *id != inst.id
            {
                sub.push(format!("last played in {name}"));
            }
            let lead = match &wd.icon {
                Some(p) => w::Lead::Image(egui::ImageSource::Uri(format!("file://{}", p.display()).into())),
                None => w::Lead::Glyph("󰕒"),
            };
            let n = listing.backups.get(&wd.folder).map_or(0, Vec::len);
            let right = if n > 0 { format!("\u{f006f} {n}") } else { String::new() };
            let r = w::rich_row(ui, &t, sel_folder == Some(wd.folder.as_str()), lead, &wd.name, &sub.join("  ·  "), &right);
            row(ui, &wd.folder, r, true);
        }
        for folder in gone {
            let n = listing.backups.get(folder).map_or(0, Vec::len);
            let sub = format!("deleted  ·  {n} backup{}", if n == 1 { "" } else { "s" });
            let r = w::rich_row_dim(ui, &t, sel_folder == Some(folder.as_str()), w::Lead::Glyph("󰕒"), folder, &sub, "", true);
            row(ui, folder, r, false);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn backup_list(
    ui: &mut Ui,
    app: &mut App,
    inst: &Instance,
    sel: Option<&World>,
    sel_folder: Option<&str>,
    snaps: &[Snapshot],
    kb: Option<Nav>,
    focused: bool,
    out: &mut Out,
) {
    let t = app.t.clone();
    let Some(folder) = sel_folder else {
        w::para(ui, RichText::new("Select a world to see its backups.").color(t.muted));
        return;
    };
    let s = &mut app.worlds;
    let pos = s.backup.as_ref().and_then(|d| snaps.iter().position(|x| &x.dir == d));
    let mut scroll = false;
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(snaps.len(), pos)) {
        s.backup = Some(snaps[p].dir.clone());
        scroll = true;
    }
    let key = |n: Nav| kb == Some(n);
    let picked = s.backup.as_ref().and_then(|d| snaps.iter().find(|x| &x.dir == d)).cloned();
    let mut back_up = false;
    ui.horizontal_wrapped(|ui| {
        // Back up now: from either pane, like the other world buttons.
        if let Some(wd) = sel
            && (w::button(ui, "back up now").clicked() || key(Nav::Add))
        {
            back_up = true;
            let _ = wd;
        }
        if let Some(snap) = &picked {
            if w::button(ui, "restore…").clicked() || key(Nav::Activate) {
                let body = if sel.is_some() {
                    format!("Put “{folder}” back as it was on {}? The world as it is now is backed up first.", snap.when())
                } else {
                    format!("Bring “{folder}” back as it was on {}?", snap.when())
                };
                app.modal = Some(Modal::Confirm(Confirm { title: "restore world".into(), body, button: "restore".into(), action: ConfirmAction::RestoreWorld(inst.id.clone(), snap.clone()) }));
            }
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                super::open_path(&snap.dir);
            }
            if w::danger_button(ui, &t, "delete…").clicked() || key(Nav::Delete) {
                app.modal = Some(Modal::Confirm(Confirm {
                    title: "delete backup".into(),
                    body: format!("Permanently delete the backup of “{folder}” from {}?", snap.when()),
                    button: "delete".into(),
                    action: ConfirmAction::DeleteBackup(snap.clone()),
                }));
            }
        }
    });
    if back_up && let Some(wd) = sel {
        match worlds_busy(app, inst) {
            Some(why) => app.toast(ToastKind::Error, format!("Can't back up while {why}")),
            None => {
                let (dir, i, name) = (wd.dir.clone(), inst.clone(), wd.name.clone());
                let rt = app.rt.clone();
                app.worlds.start(&rt, ui.ctx(), &inst.id, "back up", format!("Backing up {name}"), move || {
                    backups::snapshot_world(&dir, Owner::of(&i), "backed up by hand", false)?;
                    Ok(JobDone { select: None, note: Some(format!("Backed up {name}")) })
                });
            }
        }
    }
    ui.add_space(6.0);
    let s = &mut app.worlds;
    egui::ScrollArea::vertical().id_salt("backups").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if snaps.is_empty() {
            w::para(ui, RichText::new("No backups yet. One is taken whenever a game you played it in exits.").color(t.muted));
        }
        for snap in snaps {
            let on = s.backup.as_ref() == Some(&snap.dir);
            let r = w::rich_row(ui, &t, on, w::Lead::Glyph("\u{f006f}"), &snap.when(), &snap.info.reason, if snap.info.auto { "auto" } else { "" });
            if on && focused {
                w::focus_mark(ui, &t, r.rect);
            }
            if on && scroll {
                r.scroll_to_me(None);
            }
            if r.clicked() {
                s.backup = Some(snap.dir.clone());
                out.to_side = true;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;

    fn add_world(saves: &std::path::Path, folder: &str) {
        std::fs::create_dir_all(saves.join(folder)).unwrap();
        std::fs::write(saves.join(folder).join("level.dat"), b"").unwrap();
    }

    /// Calls `sync` like frames would until the list matches `want` worlds.
    fn wait(s: &mut WorldsState, rt: &tokio::runtime::Runtime, inst: &Instance, want: usize) {
        let ctx = egui::Context::default();
        let start = Instant::now();
        while s.sync(rt.handle(), &ctx, inst).map(|l| l.worlds.len()) != Some(want) {
            assert!(start.elapsed() < Duration::from_secs(5), "worlds never loaded");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn worlds_load_off_the_ui_thread_and_stay_up_while_refreshing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = egui::Context::default();
        let mut one = Instance::new("worlds one", "1.21.1", Loader::Vanilla);
        one.assign_id().unwrap();
        let saves = one.saves_dir();
        add_world(&saves, "a");
        let mut s = WorldsState::default();

        // The first frame doesn't read `saves/` itself; the list arrives later.
        assert!(s.sync(rt.handle(), &ctx, &one).is_none());
        wait(&mut s, &rt, &one, 1);

        // After a change the old list stays on screen until the new one lands.
        add_world(&saves, "b");
        s.invalidate();
        assert_eq!(s.sync(rt.handle(), &ctx, &one).map(|l| l.worlds.len()), Some(1));
        wait(&mut s, &rt, &one, 2);

        // A world with backups but no folder is listed as gone.
        backups::snapshot_world(&saves.join("b"), Owner::of(&one), "test", false).unwrap();
        std::fs::remove_dir_all(saves.join("b")).unwrap();
        s.invalidate();
        wait(&mut s, &rt, &one, 1);
        assert_eq!(s.sync(rt.handle(), &ctx, &one).unwrap().gone(), ["b"]);

        // Another instance never shows this one's worlds.
        let mut two = one.clone();
        two.id = format!("{}-two", one.id);
        s.selected = Some("a".into());
        assert!(s.sync(rt.handle(), &ctx, &two).is_none());
        assert!(s.selected.is_none());
        std::fs::remove_dir_all(one.dir()).unwrap();
        backups::forget_instance(&one.id);
    }
}
