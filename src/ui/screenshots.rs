//! Screenshot grid: the instance's own, or every instance's with each one's name
//! under it. The folders are listed and thumbnails decoded off the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use egui::{CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, Ui, vec2};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, Confirm, ConfirmAction, Modal, ToastKind};
use crate::instance::Instance;
use crate::task::Promise;

const MAX_DECODING: usize = 4;

enum Thumb {
    Loading,
    Ready(TextureHandle),
    Failed,
}

#[derive(Clone)]
struct Shot {
    path: PathBuf,
    modified: Option<SystemTime>,
    /// Id of the instance it belongs to.
    owner: String,
}

/// What's listed: one instance's id, or every instance's.
#[derive(Clone, PartialEq)]
enum Scope {
    One(String),
    All,
}

#[derive(Default)]
pub struct ShotsState {
    /// The listed screenshots, newest first, kept on screen while a refresh runs.
    list: Option<(Scope, Vec<Shot>)>,
    /// When `list` was read; None once it's out of date.
    fetched: Option<Instant>,
    loading: Option<(Scope, Promise<Vec<Shot>>)>,
    thumbs: Arc<Mutex<HashMap<PathBuf, Thumb>>>,
    selected: Option<PathBuf>,
}

impl ShotsState {
    pub fn invalidate(&mut self) {
        self.fetched = None;
        // A read already under way may predate the change.
        self.loading = None;
    }

    /// The screenshots in `dirs` (instance id, folder), re-read in the background
    /// every few seconds. None until the first read for this scope lands.
    fn sync(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context, scope: Scope, dirs: Vec<(String, PathBuf)>) -> Option<&[Shot]> {
        if let Some((sc, p)) = &self.loading
            && let Some(r) = p.take()
        {
            if *sc == scope {
                self.list = Some((scope.clone(), r.unwrap_or_default()));
                self.fetched = Some(Instant::now());
            }
            self.loading = None;
        }
        if self.list.as_ref().is_some_and(|(sc, _)| *sc != scope) {
            self.list = None;
            self.fetched = None;
        }
        if self.loading.as_ref().is_some_and(|(sc, _)| *sc != scope) {
            self.loading = None;
        }
        let fresh = self.fetched.is_some_and(|at| at.elapsed() < Duration::from_secs(3));
        if !fresh && self.loading.is_none() {
            let p = Promise::spawn(rt, ctx, async move { Ok(tokio::task::spawn_blocking(move || list(&dirs)).await?) });
            self.loading = Some((scope, p));
        }
        self.list.as_ref().map(|(_, l)| l.as_slice())
    }

    fn thumb(&self, path: &Path, ctx: &egui::Context, rt: &tokio::runtime::Handle) -> Option<TextureHandle> {
        let mut thumbs = self.thumbs.lock().unwrap();
        match thumbs.get(path) {
            Some(Thumb::Ready(tex)) => return Some(tex.clone()),
            Some(_) => return None,
            None => {}
        }
        if thumbs.values().filter(|t| matches!(t, Thumb::Loading)).count() >= MAX_DECODING {
            ctx.request_repaint_after(Duration::from_millis(50));
            return None;
        }
        thumbs.insert(path.to_path_buf(), Thumb::Loading);
        let (path, slot, ctx) = (path.to_path_buf(), self.thumbs.clone(), ctx.clone());
        rt.spawn_blocking(move || {
            let result = image::open(&path).map(|img| {
                let small = img.thumbnail(480, 270).to_rgba8();
                let size = [small.width() as usize, small.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, small.as_raw());
                ctx.load_texture(path.display().to_string(), color, egui::TextureOptions::LINEAR)
            });
            slot.lock().unwrap().insert(path, result.map(Thumb::Ready).unwrap_or(Thumb::Failed));
            ctx.request_repaint();
        });
        None
    }
}

fn list(dirs: &[(String, PathBuf)]) -> Vec<Shot> {
    let mut out: Vec<Shot> = dirs
        .iter()
        .flat_map(|(id, dir)| {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    n.ends_with(".png") || n.ends_with(".jpg") || n.ends_with(".jpeg")
                })
                .map(|e| Shot { path: e.path(), modified: e.metadata().and_then(|m| m.modified()).ok(), owner: id.clone() })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Put an image file on the Wayland clipboard.
fn copy_image(path: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    std::process::Command::new("wl-copy").args(["--type", "image/png"]).stdin(file).spawn().map(|_| ())
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let rt = app.rt.clone();
    let all = app.cfg.all_screenshots;
    let (scope, dirs) = if all {
        (Scope::All, app.instances.iter().map(|i| (i.id.clone(), i.screenshots_dir())).collect())
    } else {
        (Scope::One(inst.id.clone()), vec![(inst.id.clone(), inst.screenshots_dir())])
    };
    let shots = app.shots.sync(&rt, &ctx, scope, dirs).map(<[Shot]>::to_vec);
    let kb = app.nav_for(Zone::Main);
    let focused = app.focus == Zone::Main;
    let key = |n: Nav| kb == Some(n);
    let shots = shots.unwrap_or_default();
    let s = &mut app.shots;
    let mut scroll = false;
    if let Some(n) = kb.filter(|n| n.is_move()) {
        let pos = s.selected.as_ref().and_then(|p| shots.iter().position(|q| q.path == *p));
        if let Some(i) = n.step(shots.len(), pos) {
            s.selected = Some(shots[i].path.clone());
            scroll = true;
        }
    }
    let sel = s.selected.clone().filter(|p| shots.iter().any(|q| q.path == *p));

    ui.horizontal(|ui| {
        // Laid out first, from the right, so the file name can't run under them.
        let mut toggled = key(Nav::Toggle);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                // In the merged view, the selected screenshot's own instance folder.
                let dir = sel.as_ref().filter(|_| all).and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| inst.screenshots_dir());
                super::open_path(&dir);
            }
            let mut on = all;
            if ui.checkbox(&mut on, "all instances").on_hover_text("Show every instance's screenshots here").changed() {
                toggled = true;
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                buttons(app, ui, &t, sel.as_ref(), shots.len(), &key);
            });
        });
        if toggled {
            app.cfg.all_screenshots = !all;
            if let Err(e) = app.cfg.save() {
                app.toast(ToastKind::Error, format!("Couldn't save settings: {e}"));
            }
        }
    });
    ui.add_space(8.0);
    if shots.is_empty() {
        // Blank until the first read lands, rather than flashing "no screenshots".
        if app.shots.list.is_some() {
            let title = if all { "no screenshots in any instance" } else { "no screenshots yet" };
            w::empty_state(ui, &t, "󰹑", title, "Press F2 in game to take one.");
        }
        return;
    }

    let s = &mut app.shots;
    let names: HashMap<&str, &str> = app.instances.iter().map(|i| (i.id.as_str(), i.name.as_str())).collect();
    let gap = 8.0;
    let tile_w = 220.0;
    // Room for the instance's name under each tile in the merged view.
    let name_size = t.body_size() - 2.0;
    let caption = if all { name_size + 6.0 } else { 0.0 };
    let mut clicked = false;
    egui::ScrollArea::vertical().id_salt("shots").auto_shrink(false).show(ui, |ui| {
        let cols = ((ui.available_width() + gap) / (tile_w + gap)).floor().max(1.0) as usize;
        let w_each = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let h_each = w_each * 9.0 / 16.0;
        ui.spacing_mut().item_spacing = vec2(gap, gap);
        for row in shots.chunks(cols) {
            ui.horizontal(|ui| {
                for shot in row {
                    let path = &shot.path;
                    let (cell, resp) = ui.allocate_exact_size(vec2(w_each, h_each + caption), Sense::click());
                    let rect = Rect::from_min_size(cell.min, vec2(w_each, h_each));
                    if ui.is_rect_visible(cell) {
                        let p = ui.painter();
                        p.rect_filled(rect, CornerRadius::ZERO, t.bg_dark);
                        match s.thumb(path, &ctx, &rt) {
                            Some(tex) => {
                                // Fit inside the tile, keeping the aspect ratio.
                                let [tw, th] = tex.size().map(|v| v as f32);
                                let scale = (rect.width() / tw).min(rect.height() / th);
                                let r = Rect::from_center_size(rect.center(), vec2(tw * scale, th * scale));
                                p.image(tex.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                            }
                            None if matches!(s.thumbs.lock().unwrap().get(path), Some(Thumb::Failed)) => {
                                p.text(rect.center(), egui::Align2::CENTER_CENTER, "󰋩", egui::FontId::monospace(t.body_size() * 2.0), t.muted);
                            }
                            None => {}
                        }
                        let on = sel.as_ref() == Some(path);
                        let stroke = if on && focused {
                            Stroke::new(3.0, t.accent)
                        } else if on {
                            Stroke::new(2.0, t.fg_a(0.7))
                        } else if resp.hovered() {
                            Stroke::new(1.0, t.fg_a(0.4))
                        } else {
                            Stroke::new(1.0, t.fg_a(0.10))
                        };
                        p.rect_stroke(rect, CornerRadius::ZERO, stroke, StrokeKind::Inside);
                        if all {
                            let name = names.get(shot.owner.as_str()).copied().unwrap_or(&shot.owner);
                            let p = p.with_clip_rect(cell);
                            p.text(egui::pos2(rect.left(), rect.bottom() + 4.0), egui::Align2::LEFT_TOP, name, egui::FontId::monospace(name_size), t.muted);
                        }
                    }
                    if sel.as_ref() == Some(path) && scroll {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked() {
                        s.selected = Some(path.clone());
                        clicked = true;
                    }
                    if resp.double_clicked() {
                        super::open_url(&path.display().to_string());
                    }
                }
            });
        }
    });
    if clicked {
        app.focus = Zone::Main;
    }
}

/// Open, copy and delete for the selection, or the count when there's none.
fn buttons(app: &mut App, ui: &mut Ui, t: &crate::theme::Theme, sel: Option<&PathBuf>, n: usize, key: &dyn Fn(Nav) -> bool) {
    if let Some(p) = sel {
        if w::primary_button(ui, t, "open").clicked() || key(Nav::Activate) {
            super::open_url(&p.display().to_string());
        }
        if w::button(ui, "copy").clicked() || key(Nav::Copy) {
            match copy_image(p) {
                Ok(()) => app.toasts.push(super::Toast::info("Screenshot copied")),
                Err(e) => app.toasts.push(super::Toast::error(format!("Couldn't copy: {e}"))),
            }
        }
        if w::danger_button(ui, t, "delete…").clicked() || key(Nav::Delete) {
            app.modal = Some(Modal::Confirm(Confirm {
                title: "delete screenshot".into(),
                body: format!("Delete {}?", p.file_name().unwrap_or_default().to_string_lossy()),
                button: "delete".into(),
                action: ConfirmAction::DeletePath(p.clone()),
            }));
        }
        ui.add(egui::Label::new(RichText::new(p.file_name().unwrap_or_default().to_string_lossy()).color(t.muted)).truncate());
    } else {
        ui.label(RichText::new(format!("{n} screenshot{}", if n == 1 { "" } else { "s" })).color(t.muted));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(s: &mut ShotsState, rt: &tokio::runtime::Runtime, scope: Scope, dirs: &[(String, PathBuf)], want: usize) -> Vec<Shot> {
        let ctx = egui::Context::default();
        let start = Instant::now();
        loop {
            if let Some(l) = s.sync(rt.handle(), &ctx, scope.clone(), dirs.to_vec())
                && l.len() == want
            {
                return l.to_vec();
            }
            assert!(start.elapsed() < Duration::from_secs(5), "screenshots never loaded");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn merged_view_lists_every_instance_with_its_owner() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = egui::Context::default();
        let root = crate::paths::test_root().join(format!("shots-{}", uuid::Uuid::new_v4()));
        let dirs: Vec<(String, PathBuf)> = ["a", "b"].iter().map(|id| (id.to_string(), root.join(id))).collect();
        for (id, dir) in &dirs {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(format!("{id}.png")), "png").unwrap();
            // The same name in two instances is two screenshots.
            std::fs::write(dir.join("same.png"), "png").unwrap();
            std::fs::write(dir.join("notes.txt"), "not a picture").unwrap();
        }
        let mut s = ShotsState::default();

        // The first frame doesn't list folders itself; the list arrives later.
        assert!(s.sync(rt.handle(), &ctx, Scope::One("a".into()), dirs[..1].to_vec()).is_none());
        let one = wait(&mut s, &rt, Scope::One("a".into()), &dirs[..1], 2);
        assert!(one.iter().all(|x| x.owner == "a"));

        // Switching to every instance never shows the one-instance list as the merged one.
        assert!(s.sync(rt.handle(), &ctx, Scope::All, dirs.clone()).is_none());
        let all = wait(&mut s, &rt, Scope::All, &dirs, 4);
        let mut got: Vec<(String, String)> = all.iter().map(|x| (x.owner.clone(), x.path.file_name().unwrap().to_string_lossy().into_owned())).collect();
        got.sort();
        assert_eq!(got, [("a", "a.png"), ("a", "same.png"), ("b", "b.png"), ("b", "same.png")].map(|(o, n)| (o.to_string(), n.to_string())));
        std::fs::remove_dir_all(root).unwrap();
    }
}
