//! Singleplayer worlds in `saves/`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{RichText, Ui};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, Confirm, ConfirmAction, Modal, ToastKind};
use crate::instance::Instance;
use crate::launch::QuickPlay;
use crate::saves::{self, World};
use crate::task::{Promise, Task};

#[derive(Default)]
pub struct WorldsState {
    /// Instance id and its worlds, kept on screen while a refresh runs.
    list: Option<(String, Vec<World>)>,
    /// When `list` was read; None once it's out of date.
    fetched: Option<Instant>,
    /// Background read of an instance's `saves/`.
    loading: Option<(String, Promise<Vec<World>>)>,
    /// (task, new folder, instance id)
    pub dups: Vec<(Task, Promise<String>, String)>,
    selected: Option<String>,
    /// World dir being renamed, and the new name.
    rename: Option<(PathBuf, String)>,
}

impl WorldsState {
    pub fn invalidate(&mut self) {
        self.fetched = None;
        // A read already under way may predate the change.
        self.loading = None;
    }

    /// The instance's worlds, re-read in the background every few seconds.
    /// None until the first read for this instance lands.
    fn sync(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context, inst_id: &str, saves: PathBuf) -> Option<&[World]> {
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
            self.rename = None;
        }
        if self.loading.as_ref().is_some_and(|(id, _)| id != inst_id) {
            self.loading = None;
        }
        let fresh = self.fetched.is_some_and(|at| at.elapsed() < Duration::from_secs(3));
        if !fresh && self.loading.is_none() {
            let p = Promise::spawn(rt, ctx, async move { Ok(tokio::task::spawn_blocking(move || saves::list_worlds(&saves)).await?) });
            self.loading = Some((inst_id.to_string(), p));
        }
        self.list.as_ref().map(|(_, l)| l.as_slice())
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.dups.iter().map(|(t, ..)| t)
    }

    pub fn busy_with(&self, inst_id: &str) -> bool {
        self.dups.iter().any(|(.., id)| id == inst_id)
    }

    /// A finished duplicate selects the copy if its instance is still on screen.
    pub fn duplicated(&mut self, inst_id: &str, folder: String) {
        if self.list.as_ref().is_some_and(|(id, _)| id == inst_id) {
            self.selected = Some(folder);
        }
        self.invalidate();
    }
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let Some(worlds) = app.worlds.sync(&app.rt, ui.ctx(), &inst.id, inst.saves_dir()).map(<[World]>::to_vec) else {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| ui.label(RichText::new("reading worlds…").color(t.muted)));
        return;
    };
    if worlds.is_empty() {
        w::empty_state(ui, &t, "󰕒", "no worlds yet", "Create one in game, or drop a world folder into saves.");
        ui.vertical_centered(|ui| {
            if w::button(ui, "open saves folder").clicked() {
                super::open_path(&inst.saves_dir());
            }
        });
        return;
    }
    // Keyboard: move the selection, or act like the matching button.
    let kb = if app.worlds.rename.is_some() { None } else { app.nav_for(Zone::Main) };
    let focused = app.focus == Zone::Main;
    let s = &mut app.worlds;
    let pos = s.selected.as_ref().and_then(|f| worlds.iter().position(|w| &w.folder == f));
    let mut scroll = false;
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(worlds.len(), pos)) {
        s.selected = Some(worlds[p].folder.clone());
        scroll = true;
    }
    let key = |n: Nav| kb == Some(n);
    let sel = s.selected.as_ref().and_then(|f| worlds.iter().find(|w| &w.folder == f)).cloned();

    let mut play = None;
    ui.horizontal(|ui| {
        if let Some(wd) = &sel {
            if w::primary_button(ui, &t, "󰐊 play").clicked() || key(Nav::Activate) {
                play = Some(wd.folder.clone());
            }
            if w::button(ui, "rename").clicked() || key(Nav::Rename) {
                s.rename = Some((wd.dir.clone(), wd.name.clone()));
            }
            // One at a time per instance, so two copies can't pick the same folder name.
            if (w::button(ui, "duplicate").clicked() || key(Nav::Duplicate)) && !s.busy_with(&inst.id) {
                let task = Task::new("duplicate");
                task.stage(format!("Duplicating {}", wd.name), 0);
                let world = wd.clone();
                let p = Promise::spawn(&app.rt, ui.ctx(), async move { tokio::task::spawn_blocking(move || saves::duplicate_world(&world)).await? });
                s.dups.push((task, p, inst.id.clone()));
            }
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                super::open_path(&wd.dir);
            }
            if w::danger_button(ui, &t, "delete…").clicked() || key(Nav::Delete) {
                app.modal = Some(Modal::Confirm(Confirm {
                    title: "delete world".into(),
                    body: format!("Permanently delete “{}”? This can't be undone.", wd.name),
                    button: "delete".into(),
                    action: ConfirmAction::DeletePath(wd.dir.clone()),
                }));
            }
        } else {
            ui.label(RichText::new("select a world").color(t.muted));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if w::button(ui, "saves folder").clicked() {
                super::open_path(&inst.saves_dir());
            }
        });
    });

    let s = &mut app.worlds;
    if let Some((dir, name)) = &mut s.rename {
        ui.add_space(6.0);
        let mut done = None;
        ui.horizontal(|ui| {
            w::field_label(ui, &t, "new name");
            let r = w::text_input(ui, name, "", 280.0, true);
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
    let mut clicked = false;
    egui::ScrollArea::vertical().id_salt("worlds").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for wd in &worlds {
            let on = s.selected.as_deref() == Some(wd.folder.as_str());
            let mode = if wd.hardcore { "hardcore" } else { wd.mode };
            let mut sub = vec![mode.to_string()];
            if let Some(v) = &wd.version {
                sub.push(v.clone());
            }
            sub.push(w::ago(wd.last_played));
            if wd.folder != wd.name {
                sub.push(wd.folder.clone());
            }
            let lead = match &wd.icon {
                Some(p) => w::Lead::Image(egui::ImageSource::Uri(format!("file://{}", p.display()).into())),
                None => w::Lead::Glyph("󰕒"),
            };
            let r = w::rich_row(ui, &t, on, lead, &wd.name, &sub.join("  ·  "), "");
            if on && focused {
                w::focus_mark(ui, &t, r.rect);
            }
            if on && scroll {
                r.scroll_to_me(None);
            }
            if r.clicked() {
                s.selected = Some(wd.folder.clone());
                clicked = true;
            }
            if r.double_clicked() {
                play = Some(wd.folder.clone());
            }
        }
    });

    if clicked {
        app.focus = Zone::Main;
    }
    if let Some(folder) = play {
        app.launch(ui.ctx(), &inst.id, Some(QuickPlay::World(folder)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_world(saves: &std::path::Path, folder: &str) {
        std::fs::create_dir_all(saves.join(folder)).unwrap();
        std::fs::write(saves.join(folder).join("level.dat"), b"").unwrap();
    }

    /// Calls `sync` like frames would until the list matches `want` worlds.
    fn wait(s: &mut WorldsState, rt: &tokio::runtime::Runtime, id: &str, saves: &std::path::Path, want: usize) {
        let ctx = egui::Context::default();
        let start = Instant::now();
        while s.sync(rt.handle(), &ctx, id, saves.to_path_buf()).map(<[World]>::len) != Some(want) {
            assert!(start.elapsed() < Duration::from_secs(5), "worlds never loaded");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn worlds_load_off_the_ui_thread_and_stay_up_while_refreshing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = egui::Context::default();
        let saves = crate::paths::test_root().join(format!("worlds-{}", uuid::Uuid::new_v4()));
        add_world(&saves, "a");
        let mut s = WorldsState::default();

        // The first frame doesn't read `saves/` itself; the list arrives later.
        assert!(s.sync(rt.handle(), &ctx, "one", saves.clone()).is_none());
        wait(&mut s, &rt, "one", &saves, 1);

        // After a change the old list stays on screen until the new one lands.
        add_world(&saves, "b");
        s.invalidate();
        assert_eq!(s.sync(rt.handle(), &ctx, "one", saves.clone()).map(<[World]>::len), Some(1));
        wait(&mut s, &rt, "one", &saves, 2);

        // Another instance never shows this one's worlds.
        s.selected = Some("a".into());
        assert!(s.sync(rt.handle(), &ctx, "two", saves.clone()).is_none());
        assert!(s.selected.is_none());
    }
}
