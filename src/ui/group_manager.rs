//! Manage groups (g): create, rename, colour, reorder, delete, and choose
//! which instances belong to each.

use egui::{Align, CornerRadius, RichText, Sense, Stroke, StrokeKind, Ui, vec2};

use super::widgets::{self as w, Lead};
use super::nav::Nav;
use super::{App, ToastKind};
use crate::groups::{COLORS, Group};

#[derive(Default)]
pub struct GroupManager {
    selected: Option<String>,
    new_name: String,
    rename: String,
    focus_new: bool,
    focus_rename: bool,
    /// Keyboard is in the member list (else the group list).
    in_members: bool,
    member: usize,
}

impl GroupManager {
    pub fn new(selected: Option<String>) -> Self {
        let rename = selected.clone().unwrap_or_default();
        Self { focus_new: selected.is_none(), selected, rename, new_name: String::new(), ..Default::default() }
    }

    fn select(&mut self, name: Option<String>) {
        self.rename = name.clone().unwrap_or_default();
        self.selected = name;
    }
}

impl App {
    fn save_groups(&mut self) {
        if let Err(e) = self.groups.save() {
            self.toast(ToastKind::Error, format!("Couldn't save groups: {e}"));
        }
    }

    /// Point every instance in `from` at `to` ("" for no group).
    fn regroup(&mut self, from: &str, to: &str) {
        for i in self.instances.iter_mut().filter(|i| i.group == from) {
            i.group = to.to_string();
            let _ = i.save();
        }
    }
}

/// Returns true when the dialog should close.
pub fn show(app: &mut App, s: &mut GroupManager, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let screen = ui.ctx().content_rect();
    ui.set_width((screen.width() - 80.0).clamp(420.0, 780.0));
    ui.set_height((screen.height() - 120.0).clamp(300.0, 520.0));
    if s.selected.as_ref().is_some_and(|n| app.groups.get(n).is_none()) {
        s.select(None);
    }

    w::heading(ui, &t, "groups");
    ui.add_space(10.0);
    let mut close = false;

    // Keyboard: j/k groups, l into members (space toggles), h back,
    // a new, r rename, c colour, J/K reorder, x delete.
    let kb = app.modal_nav();
    let key = |n: Nav| kb == Some(n);
    match kb {
        Some(Nav::Right) if s.selected.is_some() => s.in_members = true,
        Some(Nav::Left) => s.in_members = false,
        Some(Nav::Add) => s.focus_new = true,
        Some(Nav::Rename) => s.focus_rename = true,
        _ => {}
    }
    if let Some(n) = kb.filter(|n| n.is_move()) {
        if s.in_members {
            s.member = n.step(app.instances.len(), Some(s.member)).unwrap_or(0);
        } else {
            let names = app.groups.names();
            let pos = s.selected.as_ref().and_then(|g| names.iter().position(|x| x == g));
            if let Some(p) = n.step(names.len(), pos) {
                s.select(Some(names[p].clone()));
            }
        }
    }
    let in_members = s.in_members;

    ui.columns(2, |cols| {
        // -- left: the groups
        let ui = &mut cols[0];
        w::section(ui, &t, "groups");
        let list = app.groups.list.clone();
        egui::ScrollArea::vertical().id_salt("groups").max_height(ui.available_height() - 90.0).auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if list.is_empty() {
                w::para(ui, RichText::new("no groups yet").color(t.muted));
            }
            for g in &list {
                let count = app.instances.iter().filter(|i| i.group == g.name).count();
                let tint = g.color.as_deref().and_then(|c| t.named(c));
                let on = s.selected.as_deref() == Some(g.name.as_str());
                let r = w::lead_row(ui, &t, on, Lead::Glyph("󰉋"), &g.name, &count.to_string(), tint);
                if on && !in_members {
                    w::focus_mark(ui, &t, r.rect);
                }
                if r.clicked() {
                    s.select(Some(g.name.clone()));
                    s.in_members = false;
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let r = w::text_input(ui, &mut s.new_name, "new group", ui.available_width() - 60.0, std::mem::take(&mut s.focus_new));
            let name = s.new_name.trim().to_string();
            let ok = !name.is_empty() && app.groups.get(&name).is_none();
            let add = ui.add_enabled_ui(ok, |ui| w::button(ui, "add")).inner.clicked()
                || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if add {
                app.groups.list.push(Group { name: name.clone(), color: None });
                app.save_groups();
                s.new_name.clear();
                s.select(Some(name));
            }
        });

        // -- right: the selected group
        let ui = &mut cols[1];
        let Some(name) = s.selected.clone() else {
            w::empty_state(ui, &t, "󰉋", "select or create a group", "Groups can have a colour, and instances can move between them.");
            return;
        };

        w::section(ui, &t, "name");
        ui.horizontal(|ui| {
            let r = w::text_input(ui, &mut s.rename, "", ui.available_width() - 80.0, std::mem::take(&mut s.focus_rename));
            let new = s.rename.trim().to_string();
            let ok = !new.is_empty() && new != name;
            let go = ui.add_enabled_ui(ok, |ui| w::button(ui, "rename")).inner.clicked()
                || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go {
                if app.groups.get(&new).is_some() {
                    app.toast(ToastKind::Error, format!("There's already a group called {new}"));
                } else {
                    if let Some(g) = app.groups.list.iter_mut().find(|g| g.name == name) {
                        g.name = new.clone();
                    }
                    app.regroup(&name, &new);
                    for c in app.cfg.collapsed_groups.iter_mut().filter(|c| **c == name) {
                        *c = new.clone();
                    }
                    let _ = app.cfg.save();
                    app.save_groups();
                    s.select(Some(new));
                }
            }
        });
        let name = s.selected.clone().unwrap();

        ui.add_space(6.0);
        w::section(ui, &t, "colour");
        let current = app.groups.color(&name).map(String::from);
        let mut pick: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let size = vec2(26.0, 26.0);
            for c in std::iter::once(None).chain(COLORS.iter().map(|c| Some(c.to_string()))) {
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                let p = ui.painter();
                match c.as_deref().and_then(|c| t.named(c)) {
                    Some(col) => {
                        p.rect_filled(rect.shrink(3.0), CornerRadius::ZERO, col);
                    }
                    None => {
                        p.rect_stroke(rect.shrink(3.0), CornerRadius::ZERO, Stroke::new(1.0, t.muted), StrokeKind::Inside);
                        p.line_segment([rect.shrink(6.0).left_bottom(), rect.shrink(6.0).right_top()], Stroke::new(1.0, t.muted));
                    }
                }
                if c == current {
                    p.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(2.0, t.fg_light), StrokeKind::Inside);
                }
                let resp = resp.on_hover_text(c.as_deref().unwrap_or("none")).on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    pick = Some(c);
                }
            }
        });
        if key(Nav::Cycle) {
            let i = current.as_deref().and_then(|c| COLORS.iter().position(|x| *x == c));
            pick = Some(match i {
                None => Some(COLORS[0].to_string()),
                Some(i) if i + 1 < COLORS.len() => Some(COLORS[i + 1].to_string()),
                Some(_) => None,
            });
        }
        if let Some(c) = pick {
            if let Some(g) = app.groups.list.iter_mut().find(|g| g.name == name) {
                g.color = c;
            }
            app.save_groups();
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let i = app.groups.list.iter().position(|g| g.name == name).unwrap_or(0);
            let n = app.groups.list.len();
            if ui.add_enabled_ui(i > 0, |ui| w::button(ui, "↑")).inner.on_hover_text("move up").clicked() || (i > 0 && key(Nav::MoveUp)) {
                app.groups.list.swap(i, i - 1);
                app.save_groups();
            }
            if ui.add_enabled_ui(i + 1 < n, |ui| w::button(ui, "↓")).inner.on_hover_text("move down").clicked() || (i + 1 < n && key(Nav::MoveDown)) {
                app.groups.list.swap(i, i + 1);
                app.save_groups();
            }
            if w::danger_button(ui, &t, "delete group").on_hover_text("its instances stay, ungrouped").clicked() || (!in_members && key(Nav::Delete)) {
                app.groups.list.retain(|g| g.name != name);
                app.regroup(&name, "");
                app.cfg.collapsed_groups.retain(|c| *c != name);
                let _ = app.cfg.save();
                app.save_groups();
                s.select(None);
            }
        });
        if s.selected.is_none() {
            return;
        }

        ui.add_space(6.0);
        w::section(ui, &t, "instances");
        egui::ScrollArea::vertical().id_salt("members").max_height(ui.available_height() - 50.0).auto_shrink(false).show(ui, |ui| {
            let mut changed = None;
            for (n, inst) in app.instances.iter().enumerate() {
                let mut member = inst.group == name;
                let cursor = in_members && s.member == n;
                if cursor && key(Nav::Toggle) {
                    changed = Some((n, !member));
                }
                let row = ui.horizontal(|ui| {
                    if ui.checkbox(&mut member, "").changed() {
                        changed = Some((n, member));
                    }
                    let mut text = egui::text::LayoutJob::default();
                    RichText::new(&inst.name).color(t.fg).append_to(&mut text, ui.style(), egui::FontSelection::Default, Align::Center);
                    if !member && !inst.group.is_empty() {
                        RichText::new(format!("  in {}", inst.group)).color(t.muted).append_to(&mut text, ui.style(), egui::FontSelection::Default, Align::Center);
                    }
                    if ui.add(egui::Label::new(text).truncate().sense(Sense::click())).clicked() {
                        changed = Some((n, !member));
                    }
                });
                if cursor {
                    w::focus_mark(ui, &t, row.response.rect);
                    if kb.is_some_and(|n| n.is_move()) {
                        row.response.scroll_to_me(None);
                    }
                }
            }
            if let Some((n, member)) = changed {
                let i = &mut app.instances[n];
                i.group = if member { name.clone() } else { String::new() };
                let _ = i.save();
            }
        });
    });

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
        if w::button(ui, "close").clicked() {
            close = true;
        }
    });
    close
}
