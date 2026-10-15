//! Choosing an instance icon.

use std::path::PathBuf;

use egui::{CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, Ui, vec2};

use super::widgets as w;
use super::nav::Nav;
use super::{App, ToastKind};
use crate::icons;
use crate::task::Promise;

pub struct IconPicker {
    pub id: String,
    add_p: Option<Promise<Option<PathBuf>>>,
    /// Keyboard cursor in the grid, as a key.
    cursor: Option<String>,
}

impl IconPicker {
    pub fn new(id: String) -> Self {
        Self { id, add_p: None, cursor: None }
    }
}

/// Returns true when the picker should close.
pub fn show(app: &mut App, s: &mut IconPicker, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let Some(current) = app.instances.iter().find(|i| i.id == s.id).map(|i| i.icon.clone()) else { return true };
    let mut choose = None;
    let mut remove = None;

    if let Some(p) = &s.add_p
        && let Some(r) = p.take()
    {
        s.add_p = None;
        match r.and_then(|path| path.map(|p| icons::add_custom(&p)).transpose()) {
            Ok(Some(key)) => choose = Some(key),
            Ok(None) => {}
            Err(e) => app.toast(ToastKind::Error, format!("Couldn't add icon: {e:#}")),
        }
    }

    ui.set_width(560.0);
    w::heading(ui, &t, "instance icon");
    ui.add_space(10.0);

    let tile = 56.0;
    let builtin: Vec<String> = icons::BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
    let custom = icons::custom();
    let all: Vec<String> = builtin.iter().chain(custom.iter()).cloned().collect();
    // Keyboard: hjkl over the grid, ⏎ to choose.
    let cols = ((ui.available_width() + 8.0) / (tile + 8.0)).floor().max(1.0) as isize;
    let cursor = s.cursor.clone().unwrap_or_else(|| current.clone());
    let at = all.iter().position(|k| *k == cursor).unwrap_or(0) as isize;
    let nav = app.modal_nav();
    let moved = match nav {
        Some(Nav::Left) => Some(at - 1),
        Some(Nav::Right) => Some(at + 1),
        Some(Nav::Down) => Some(at + cols),
        Some(Nav::Up) => Some(at - cols),
        Some(Nav::Top) => Some(0),
        Some(Nav::Bottom) => Some(all.len() as isize - 1),
        _ => None,
    };
    if let Some(i) = moved {
        s.cursor = Some(all[i.clamp(0, all.len() as isize - 1) as usize].clone());
    }
    let cursor = s.cursor.clone();
    if let Some(c) = &cursor
        && (nav == Some(Nav::Activate) || ui.input(|i| i.key_pressed(egui::Key::Enter)))
    {
        choose = Some(c.clone());
    }
    let mut grid = |ui: &mut Ui, keys: &[String], removable: bool| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            for key in keys {
                let (rect, resp) = ui.allocate_exact_size(vec2(tile, tile), Sense::click());
                let on = *key == current;
                if on || resp.hovered() {
                    let fill = t.fg_a(if on { t.menu_selected_bg_alpha } else { t.hover_fill_alpha });
                    let stroke = if on { Stroke::new(2.0, t.accent) } else { Stroke::NONE };
                    ui.painter().rect(rect, CornerRadius::ZERO, fill, stroke, StrokeKind::Inside);
                }
                let inner = Rect::from_center_size(rect.center(), vec2(tile - 14.0, tile - 14.0));
                egui::Image::new(icons::source(key)).corner_radius(7.0).paint_at(ui, inner);
                if cursor.as_ref() == Some(key) {
                    ui.painter().rect_stroke(rect, CornerRadius::ZERO, Stroke::new(2.0, t.fg_light), StrokeKind::Inside);
                }
                let resp = resp.on_hover_text(key.trim_start_matches("custom:")).on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    choose = Some(key.clone());
                }
                if removable {
                    resp.context_menu(|ui| {
                        if ui.button(RichText::new("remove icon").color(t.red)).clicked() {
                            remove = Some(key.clone());
                        }
                    });
                }
            }
        });
    };

    w::section(ui, &t, "built in  ·  hjkl to move, 󰌑 to choose");
    grid(ui, &builtin, false);
    if !custom.is_empty() {
        ui.add_space(8.0);
        w::section(ui, &t, "yours  ·  right-click to remove");
        grid(ui, &custom, true);
    }

    ui.add_space(16.0);
    let mut close = false;
    ui.horizontal(|ui| {
        let busy = s.add_p.is_some();
        if ui.add_enabled_ui(!busy, |ui| w::button(ui, "add image…")).inner.clicked() {
            s.add_p = Some(Promise::spawn(&app.rt, ui.ctx(), async {
                let f = rfd::AsyncFileDialog::new()
                    .set_title("Choose an icon")
                    .add_filter("Images", &["png", "jpg", "jpeg", "svg"])
                    .pick_file()
                    .await;
                Ok(f.map(|f| f.path().to_path_buf()))
            }));
        }
        if busy {
            ui.spinner();
        }
        if w::button(ui, "close").clicked() {
            close = true;
        }
    });

    if let Some(key) = remove {
        icons::remove_custom(&key);
        for i in app.instances.iter_mut().filter(|i| i.icon == key) {
            i.icon = icons::DEFAULT.into();
            let _ = i.save();
        }
    }
    if let Some(key) = choose
        && let Some(i) = app.instances.iter_mut().find(|i| i.id == s.id)
    {
        i.icon = key;
        if let Err(e) = i.save() {
            app.toast(ToastKind::Error, format!("Save failed: {e}"));
        }
        close = true;
    }
    close
}
