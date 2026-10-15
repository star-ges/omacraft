//! Screenshot grid. Thumbnails are decoded and shrunk off the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use egui::{CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, Ui, vec2};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, Confirm, ConfirmAction, Modal};
use crate::instance::Instance;

const MAX_DECODING: usize = 4;

enum Thumb {
    Loading,
    Ready(TextureHandle),
    Failed,
}

#[derive(Default)]
pub struct ShotsState {
    cache: Option<(String, Vec<(PathBuf, Option<SystemTime>)>, Instant)>,
    thumbs: Arc<Mutex<HashMap<PathBuf, Thumb>>>,
    selected: Option<PathBuf>,
}

impl ShotsState {
    pub fn invalidate(&mut self) {
        self.cache = None;
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

fn list(dir: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n.ends_with(".png") || n.ends_with(".jpg") || n.ends_with(".jpeg")
        })
        .map(|e| (e.path(), e.metadata().and_then(|m| m.modified()).ok()))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
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
    let s = &mut app.shots;
    let fresh = s.cache.as_ref().is_some_and(|(id, _, at)| *id == inst.id && at.elapsed() < Duration::from_secs(3));
    if !fresh {
        s.cache = Some((inst.id.clone(), list(&inst.screenshots_dir()), Instant::now()));
    }
    let shots = s.cache.as_ref().unwrap().1.clone();
    if shots.is_empty() {
        w::empty_state(ui, &t, "󰹑", "no screenshots yet", "Press F2 in game to take one.");
        return;
    }
    let kb = app.nav_for(Zone::Main);
    let focused = app.focus == Zone::Main;
    let s = &mut app.shots;
    let mut scroll = false;
    if let Some(n) = kb.filter(|n| n.is_move()) {
        let pos = s.selected.as_ref().and_then(|p| shots.iter().position(|(q, _)| q == p));
        if let Some(i) = n.step(shots.len(), pos) {
            s.selected = Some(shots[i].0.clone());
            scroll = true;
        }
    }
    let key = |n: Nav| kb == Some(n);
    let sel = s.selected.clone().filter(|p| shots.iter().any(|(q, _)| q == p));

    ui.horizontal(|ui| {
        if let Some(p) = &sel {
            if w::primary_button(ui, &t, "open").clicked() || key(Nav::Activate) {
                super::open_url(&p.display().to_string());
            }
            if w::button(ui, "copy").clicked() || key(Nav::Copy) {
                match copy_image(p) {
                    Ok(()) => app.toasts.push(super::Toast::info("Screenshot copied")),
                    Err(e) => app.toasts.push(super::Toast::error(format!("Couldn't copy: {e}"))),
                }
            }
            if w::danger_button(ui, &t, "delete…").clicked() || key(Nav::Delete) {
                app.modal = Some(Modal::Confirm(Confirm {
                    title: "delete screenshot".into(),
                    body: format!("Delete {}?", p.file_name().unwrap_or_default().to_string_lossy()),
                    button: "delete".into(),
                    action: ConfirmAction::DeletePath(p.clone()),
                }));
            }
            ui.label(RichText::new(p.file_name().unwrap_or_default().to_string_lossy()).color(t.muted));
        } else {
            let n = shots.len();
            ui.label(RichText::new(format!("{n} screenshot{}", if n == 1 { "" } else { "s" })).color(t.muted));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                super::open_path(&inst.screenshots_dir());
            }
        });
    });
    ui.add_space(8.0);

    let s = &mut app.shots;
    let gap = 8.0;
    let tile_w = 220.0;
    let mut clicked = false;
    egui::ScrollArea::vertical().id_salt("shots").auto_shrink(false).show(ui, |ui| {
        let cols = ((ui.available_width() + gap) / (tile_w + gap)).floor().max(1.0) as usize;
        let w_each = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let h_each = w_each * 9.0 / 16.0;
        ui.spacing_mut().item_spacing = vec2(gap, gap);
        for row in shots.chunks(cols) {
            ui.horizontal(|ui| {
                for (path, _) in row {
                    let (rect, resp) = ui.allocate_exact_size(vec2(w_each, h_each), Sense::click());
                    if ui.is_rect_visible(rect) {
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
