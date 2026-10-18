use egui::{Align, Layout, RichText, Sense, Ui, vec2};

use super::widgets::{self as w, Lead};
use super::{App, Confirm, ConfirmAction, Modal, ToastKind, content, logs, new_instance, screenshots, servers, version, worlds};
use super::nav::{Nav, Zone};
use crate::icons;
use crate::instance::{self, ContentKind, Instance};
use crate::shares::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstTab {
    Log,
    Version,
    Mods,
    ResourcePacks,
    ShaderPacks,
    Notes,
    Worlds,
    Servers,
    Screenshots,
    LogHistory,
}

impl InstTab {
    pub const ALL: [(InstTab, &'static str); 10] = [
        (InstTab::Version, "version"),
        (InstTab::Log, "log"),
        (InstTab::Mods, "mods"),
        (InstTab::ResourcePacks, "resource packs"),
        (InstTab::ShaderPacks, "shader packs"),
        (InstTab::Notes, "notes"),
        (InstTab::Worlds, "worlds"),
        (InstTab::Servers, "servers"),
        (InstTab::Screenshots, "screenshots"),
        (InstTab::LogHistory, "log history"),
    ];

    pub fn content_kind(self) -> Option<ContentKind> {
        match self {
            InstTab::Mods => Some(ContentKind::Mods),
            InstTab::ResourcePacks => Some(ContentKind::ResourcePacks),
            InstTab::ShaderPacks => Some(ContentKind::ShaderPacks),
            _ => None,
        }
    }

    /// Next/previous tab, wrapping.
    pub fn step(self, delta: isize) -> InstTab {
        let n = Self::ALL.len() as isize;
        let i = Self::ALL.iter().position(|(t, _)| *t == self).unwrap() as isize;
        Self::ALL[((i + delta).rem_euclid(n)) as usize].0
    }
}

/// Drag-and-drop payload: the id of the instance being dragged.
struct DragInst(String);

/// A line in the instance list.
pub enum ListRow {
    Group { name: String, count: usize, collapsed: bool },
    Instance(usize),
}

enum RowAction {
    Select(String),
    Launch(String),
    Edit(String),
    Icon(String),
    Folder(String),
    Export(String),
    SetGroup(String, String),
    Delete(String),
    ToggleGroup(String),
    ManageGroups(Option<String>),
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();

    if app.instances.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.28);
            ui.add(egui::Image::new(icons::source(icons::DEFAULT)).fit_to_exact_size(vec2(72.0, 72.0)));
            ui.add_space(10.0);
            w::heading(ui, &t, "no instances yet");
            ui.label(RichText::new("an instance is a self-contained minecraft install").color(t.muted));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let total = 300.0;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if w::primary_button(ui, &t, "  new instance  ").clicked() {
                    app.modal = Some(Modal::NewInstance(Box::new(new_instance::NewInstance::new(&app.cfg))));
                }
                if w::button(ui, "  import…  ").clicked() {
                    app.modal = Some(Modal::NewInstance(Box::new(new_instance::NewInstance::import(&app.cfg, String::new()))));
                }
            });
        });
        return;
    }

    egui::Panel::left("instance-list")
        .exact_size((ui.available_width() * 0.3).clamp(200.0, 300.0))
        .resizable(false)
        .frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 10, right: 10, top: 12, bottom: 10 }))
        .show(ui, |ui| {
            let r = ui.max_rect();
            ui.painter().vline(r.right() + 10.0, r.y_range(), egui::Stroke::new(1.0, t.fg_a(0.12)));
            ui.horizontal(|ui| {
                let focus = std::mem::take(&mut app.focus_filter);
                let resp = w::text_input(ui, &mut app.filter, "/ filter", ui.available_width() - 40.0, focus);
                if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    app.filter.clear();
                    resp.surrender_focus();
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // Enter in the filter selects the first match.
                    if let Some(&n) = app.visible_instances().first() {
                        app.selected = Some(app.instances[n].id.clone());
                    }
                }
                if w::button(ui, "+").on_hover_text("new or import (n / i)").clicked() {
                    app.modal = Some(Modal::NewInstance(Box::new(new_instance::NewInstance::new(&app.cfg))));
                }
            });
            ui.add_space(8.0);
            let actions = list(app, ui);
            for a in actions {
                apply(app, &ctx, a);
            }
        });

    let Some(inst) = app.selected_instance().cloned() else {
        ui.centered_and_justified(|ui| ui.label(RichText::new("select an instance").color(t.muted)));
        return;
    };
    detail(app, ui, &inst);
}

fn list(app: &mut App, ui: &mut Ui) -> Vec<RowAction> {
    let t = app.t.clone();
    let rows = app.list_rows();
    let groups = app.groups.names();
    let tint = |g: &str| app.groups.color(g).and_then(|c| t.named(c));
    let mut buf = std::mem::take(&mut app.group_buf);
    let mut actions = Vec::new();
    let ctx = ui.ctx().clone();
    let scroll = std::mem::take(&mut app.scroll_list);
    let focused = app.focus == Zone::List;
    let dragging = egui::DragAndDrop::payload::<DragInst>(&ctx)
        .and_then(|p| app.instances.iter().find(|i| i.id == p.0).cloned());
    // Each group's rows, to outline the one being dropped on.
    let mut blocks: Vec<(String, egui::Rect)> = Vec::new();
    let mut hover_target: Option<String> = None;
    // Treat a row as a drop target for `group`.
    let drop_on = |r: &egui::Response, group: &str, actions: &mut Vec<RowAction>, blocks: &mut Vec<(String, egui::Rect)>, hover: &mut Option<String>| {
        blocks.push((group.to_string(), r.rect));
        let Some(d) = &dragging else { return };
        if r.dnd_hover_payload::<DragInst>().is_some() && d.group != group {
            *hover = Some(group.to_string());
        }
        if let Some(p) = r.dnd_release_payload::<DragInst>()
            && d.group != group
        {
            actions.push(RowAction::SetGroup(p.0.clone(), group.to_string()));
        }
    };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for row in &rows {
            match row {
                ListRow::Group { name, count, collapsed } => {
                    ui.add_space(4.0);
                    let r = w::group_row(ui, &t, name, *count, *collapsed, tint(name));
                    drop_on(&r, name, &mut actions, &mut blocks, &mut hover_target);
                    let g = name.clone();
                    r.context_menu(|ui| {
                        if ui.button("manage groups…").clicked() {
                            actions.push(RowAction::ManageGroups(Some(g.clone())));
                        }
                    });
                    if r.clicked() {
                        actions.push(RowAction::ToggleGroup(name.clone()));
                    }
                }
                ListRow::Instance(n) => {
                    let inst = &app.instances[*n];
                    let sel = app.selected.as_deref() == Some(inst.id.as_str());
                    let right = if app.is_running(&inst.id) { "● running" } else { inst.mc_version.as_str() };
                    let r = w::drag_row(ui, &t, sel, Lead::Image(icons::source(&inst.icon)), &inst.name, right, tint(&inst.group));
                    r.dnd_set_drag_payload(DragInst(inst.id.clone()));
                    drop_on(&r, &inst.group, &mut actions, &mut blocks, &mut hover_target);
                    if sel && scroll {
                        r.scroll_to_me(None);
                    }
                    if sel && focused {
                        w::focus_mark(ui, &t, r.rect);
                    }
                    if r.clicked() {
                        actions.push(RowAction::Select(inst.id.clone()));
                    }
                    if r.double_clicked() {
                        actions.push(RowAction::Launch(inst.id.clone()));
                    }
                    let id = inst.id.clone();
                    let current = inst.group.clone();
                    r.context_menu(|ui| {
                        actions.push(RowAction::Select(id.clone()));
                        if ui.button("󰐊 play").clicked() {
                            actions.push(RowAction::Launch(id.clone()));
                        }
                        if ui.button("edit…").clicked() {
                            actions.push(RowAction::Edit(id.clone()));
                        }
                        if ui.button("change icon…").clicked() {
                            actions.push(RowAction::Icon(id.clone()));
                        }
                        if ui.button("open folder").clicked() {
                            actions.push(RowAction::Folder(id.clone()));
                        }
                        if ui.button("export…").clicked() {
                            actions.push(RowAction::Export(id.clone()));
                        }
                        ui.menu_button("move to group", |ui| {
                            for g in &groups {
                                if *g != current && ui.button(g).clicked() {
                                    actions.push(RowAction::SetGroup(id.clone(), g.clone()));
                                }
                            }
                            if !current.is_empty() && ui.button(RichText::new("no group").color(t.muted)).clicked() {
                                actions.push(RowAction::SetGroup(id.clone(), String::new()));
                            }
                            if ui.button("manage groups…").clicked() {
                                actions.push(RowAction::ManageGroups(Some(current.clone()).filter(|g| !g.is_empty())));
                            }
                            ui.separator();
                            let r = ui.add(egui::TextEdit::singleline(&mut buf).hint_text("new group").desired_width(160.0));
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !buf.trim().is_empty() {
                                actions.push(RowAction::SetGroup(id.clone(), buf.trim().to_string()));
                                buf.clear();
                                ui.close();
                            }
                        });
                        ui.separator();
                        if ui.button(RichText::new("delete…").color(t.red)).clicked() {
                            actions.push(RowAction::Delete(id.clone()));
                        }
                    });
                }
            }
        }

        // A strip to drop on to leave a group, while dragging a grouped instance.
        if let Some(d) = dragging.as_ref().filter(|d| !d.group.is_empty()) {
            ui.add_space(8.0);
            let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), w::row_height(&t) + 8.0), Sense::hover());
            let on = r.dnd_hover_payload::<DragInst>().is_some();
            let color = if on { t.fg } else { t.muted };
            ui.painter().rect_stroke(rect, 0.0, egui::Stroke::new(1.0, if on { t.fg_a(0.6) } else { t.fg_a(0.2) }), egui::StrokeKind::Inside);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("drop here to remove from {}", d.group), egui::FontId::monospace(t.body_size() - 1.0), color);
            if let Some(p) = r.dnd_release_payload::<DragInst>() {
                actions.push(RowAction::SetGroup(p.0.clone(), String::new()));
            }
        }

        if let Some(g) = &hover_target {
            let rect = blocks.iter().filter(|(b, _)| b == g).map(|(_, r)| *r).reduce(|a, b| a.union(b));
            if let Some(rect) = rect {
                let color = tint(g).unwrap_or(t.accent);
                ui.painter().rect_stroke(rect.expand(2.0), 0.0, egui::Stroke::new(2.0, color), egui::StrokeKind::Outside);
            }
        }
    });

    // What's being dragged follows the pointer.
    if let Some(d) = &dragging {
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        if let Some(pos) = ctx.pointer_interact_pos() {
            egui::Area::new(egui::Id::new("drag-ghost"))
                .order(egui::Order::Tooltip)
                .fixed_pos(pos + egui::vec2(14.0, 10.0))
                .interactable(false)
                .show(&ctx, |ui| {
                    egui::Frame::new().fill(t.bg).stroke(egui::Stroke::new(1.0, t.border_active)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let s = t.body_size() + 4.0;
                            ui.add(egui::Image::new(icons::source(&d.icon)).fit_to_exact_size(egui::vec2(s, s)));
                            ui.label(RichText::new(&d.name).color(t.fg));
                        });
                    });
                });
        }
    }
    app.group_buf = buf;
    actions
}

fn apply(app: &mut App, ctx: &egui::Context, a: RowAction) {
    let find = |app: &App, id: &str| app.instances.iter().find(|i| i.id == id).cloned();
    match a {
        RowAction::Select(id) => {
            app.selected = Some(id);
            app.focus = Zone::List;
        }
        RowAction::Launch(id) => {
            app.selected = Some(id.clone());
            app.launch(ctx, &id, None);
        }
        RowAction::Edit(id) => {
            if let Some(i) = find(app, &id) {
                app.modal = Some(Modal::Edit(Box::new(EditState::from(&i))));
            }
        }
        RowAction::Icon(id) => app.modal = Some(Modal::IconPicker(Box::new(super::icon_picker::IconPicker::new(id)))),
        RowAction::Folder(id) => {
            if let Some(i) = find(app, &id) {
                super::open_path(&i.game_dir());
            }
        }
        RowAction::Export(id) => app.start_export_dialog(ctx, &id),
        RowAction::SetGroup(id, group) => {
            if let Some(i) = app.instances.iter_mut().find(|i| i.id == id) {
                i.group = group;
                if let Err(e) = i.save() {
                    app.toast(ToastKind::Error, format!("Save failed: {e}"));
                }
            }
        }
        RowAction::Delete(id) => app.confirm_delete_instance(&id),
        RowAction::ManageGroups(g) => app.modal = Some(Modal::Groups(Box::new(super::group_manager::GroupManager::new(g)))),
        RowAction::ToggleGroup(name) => {
            let c = &mut app.cfg.collapsed_groups;
            if let Some(p) = c.iter().position(|g| *g == name) {
                c.remove(p);
            } else {
                c.push(name);
            }
            let _ = app.cfg.save();
        }
    }
}

fn detail(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 18, top: 16, bottom: 12 }).show(ui, |ui| {
        let disk = app.sizes.get(inst, &app.rt, ui.ctx());
        let tint = app.groups.color(&inst.group).and_then(|c| t.named(c));
        // Below this width the buttons move under the title instead of
        // squeezing it.
        let stacked = ui.available_width() < 620.0;
        let buttons = |app: &mut App, ui: &mut Ui| {
            if w::button(ui, "folder").clicked() {
                super::open_path(&inst.game_dir());
            }
            if w::button(ui, "edit").clicked() {
                app.modal = Some(Modal::Edit(Box::new(EditState::from(inst))));
            }
            if app.is_running(&inst.id) {
                if w::danger_button(ui, &t, "■ stop").clicked() {
                    app.kill(&inst.id);
                }
            } else if w::primary_button(ui, &t, "󰐊 play").clicked() {
                app.launch(ui.ctx(), &inst.id, None);
            }
        };
        let summary = |app: &mut App, ui: &mut Ui| {
            let size = t.body_size() * 3.6;
            let icon = ui
                .add(egui::Image::new(icons::source(&inst.icon)).fit_to_exact_size(vec2(size, size)).corner_radius(size * 0.18).sense(Sense::click()))
                .on_hover_text("change icon")
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if icon.clicked() {
                app.modal = Some(Modal::IconPicker(Box::new(super::icon_picker::IconPicker::new(inst.id.clone()))));
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                let line = |ui: &mut Ui, text: RichText| ui.add(egui::Label::new(text).truncate());
                line(ui, RichText::new(&inst.name).font(crate::theme::bold(t.body_size() * 1.6)).color(t.fg_light));
                let mut sub = egui::text::LayoutJob::default();
                let push = |job: &mut egui::text::LayoutJob, text: String, color| {
                    RichText::new(text).color(color).append_to(job, ui.style(), egui::FontSelection::Default, Align::Center)
                };
                push(&mut sub, format!("minecraft {}  ·  {}", inst.mc_version, inst.loader.display()), t.fg);
                if !inst.group.is_empty() {
                    push(&mut sub, "  ·  ".into(), t.fg);
                    push(&mut sub, format!("󰉋 {}", inst.group), tint.unwrap_or(t.fg));
                }
                if !inst.share_set.is_empty() {
                    push(&mut sub, format!("  ·  \u{f0c1} {}", inst.share_set), t.fg);
                }
                if !inst.world_set.is_empty() {
                    push(&mut sub, format!("  ·  \u{f0ac} {}", inst.world_set), t.fg);
                }
                ui.add(egui::Label::new(sub).truncate());
                let size = disk.map(w::size_label).unwrap_or_else(|| "measuring…".into());
                let stats = format!("played {}  ·  last played {}  ·  {size}", instance::format_duration(inst.played_secs), w::ago(inst.last_played));
                line(ui, RichText::new(stats).color(t.muted));
            });
        };
        if stacked {
            ui.horizontal(|ui| summary(app, ui));
            ui.add_space(8.0);
            // Same left-to-right order as the wide layout shows them.
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| buttons(app, ui));
            });
        } else {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    buttons(app, ui);
                    ui.add_space(12.0);
                    ui.with_layout(Layout::left_to_right(Align::TOP), |ui| summary(app, ui));
                });
            });
        }
        super::mod_sets::banner(app, ui, inst);
        ui.add_space(14.0);
        w::tab_strip(ui, &t, &mut app.inst_tab, &InstTab::ALL);

        match app.inst_tab {
            InstTab::Log => logs::live(app, ui, inst),
            InstTab::Version => version::show(app, ui, inst),
            InstTab::Notes => notes(app, ui, inst),
            InstTab::Worlds => worlds::show(app, ui, inst),
            InstTab::Servers => servers::show(app, ui, inst),
            InstTab::Screenshots => screenshots::show(app, ui, inst),
            InstTab::LogHistory => logs::history(app, ui, inst),
            tab => content::show(app, ui, inst, tab.content_kind().unwrap()),
        }
    });
}

fn notes(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let write = app.nav_for(Zone::Main) == Some(Nav::Activate);
    let focused = app.focus == Zone::Main;
    let Some(i) = app.instances.iter_mut().find(|i| i.id == inst.id) else { return };
    let edit = egui::TextEdit::multiline(&mut i.notes)
        .hint_text("Anything worth remembering: seeds, coordinates, server addresses, what you changed…")
        .margin(vec2(10.0, 8.0))
        .desired_width(f32::INFINITY);
    let stroke = if focused { egui::Stroke::new(1.0, t.accent) } else { egui::Stroke::NONE };
    let r = egui::Frame::new().fill(t.bg_dark).stroke(stroke).show(ui, |ui| ui.add_sized(ui.available_size(), edit)).inner;
    if write {
        r.request_focus();
    }
    if r.clicked() || r.has_focus() {
        app.focus = Zone::Main;
    }
    if r.changed()
        && let Err(e) = i.save()
    {
        app.toast(ToastKind::Error, format!("Couldn't save notes: {e}"));
    }
}

// ---------------------------------------------------------------------------

pub struct EditState {
    pub id: String,
    name: String,
    group: String,
    /// The share set and the world set, in `Kind::ALL` order.
    sets: [SetField; 2],
    memory_mb: u32,
    custom_memory: bool,
    jvm_args: String,
    java_path: String,
    custom_window: bool,
    window: (u32, u32),
}

/// A share set or world set field in the edit dialog.
struct SetField {
    kind: Kind,
    value: String,
    /// The set when the dialog opened, and what leaving it would copy.
    old: String,
    leave_size: Option<crate::task::Promise<u64>>,
    leave_bytes: Option<u64>,
}

impl SetField {
    fn new(kind: Kind, i: &Instance) -> Self {
        Self { kind, value: kind.set_of(i).to_string(), old: kind.set_of(i).to_string(), leave_size: None, leave_bytes: None }
    }
}

impl EditState {
    pub fn from(i: &Instance) -> Self {
        Self {
            id: i.id.clone(),
            name: i.name.clone(),
            group: i.group.clone(),
            sets: Kind::ALL.map(|k| SetField::new(k, i)),
            memory_mb: i.memory_mb.unwrap_or(4096),
            custom_memory: i.memory_mb.is_some(),
            jvm_args: i.jvm_args.clone(),
            java_path: i.java_path.clone(),
            custom_window: i.window.is_some(),
            window: i.window.unwrap_or((1280, 720)),
        }
    }
}

pub enum EditOutcome {
    Keep,
    Close,
    Delete,
}

pub fn edit(app: &mut App, s: &mut EditState, ui: &mut Ui) -> EditOutcome {
    let t = app.t.clone();
    let activate = app.modal_nav() == Some(Nav::Activate);
    ui.set_width(520.0);
    w::heading(ui, &t, "edit instance");
    ui.add_space(10.0);

    let field_w = 340.0;
    let groups = app.groups.names();
    let sets = Kind::ALL.map(|k| app.all_sets(k));
    egui::Grid::new("edit-grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label(RichText::new("name").color(t.muted));
        w::text_input(ui, &mut s.name, "", field_w, false);
        ui.end_row();

        ui.label(RichText::new("group").color(t.muted));
        new_instance::group_field(ui, &t, &mut s.group, &groups, "no group", field_w);
        ui.end_row();

        ui.label(RichText::new("share set").color(t.muted))
            .on_hover_text("instances in the same share set share resource packs, shader packs and schematics");
        new_instance::group_field(ui, &t, &mut s.sets[0].value, &sets[0], "not shared", field_w);
        ui.end_row();

        ui.label(RichText::new("world set").color(t.muted))
            .on_hover_text("instances in the same world set share their worlds; they must play the same minecraft version and loader");
        new_instance::group_field(ui, &t, &mut s.sets[1].value, &sets[1], "not shared", field_w);
        ui.end_row();

        ui.label(RichText::new("memory").color(t.muted));
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.custom_memory, "");
            if s.custom_memory {
                ui.add(egui::Slider::new(&mut s.memory_mb, 1024..=32768).step_by(512.0).suffix(" MiB"));
            } else {
                ui.label(RichText::new(format!("default ({} MiB)", app.cfg.default_memory_mb)).color(t.muted));
            }
        });
        ui.end_row();

        ui.label(RichText::new("window").color(t.muted));
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.custom_window, "");
            if s.custom_window {
                ui.add(egui::DragValue::new(&mut s.window.0).range(320..=7680));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut s.window.1).range(240..=4320));
            } else {
                ui.label(RichText::new("game default").color(t.muted));
            }
        });
        ui.end_row();

        ui.label(RichText::new("jvm args").color(t.muted));
        w::text_input(ui, &mut s.jvm_args, "-XX:+UseZGC", field_w, false);
        ui.end_row();

        ui.label(RichText::new("java").color(t.muted));
        w::text_input(ui, &mut s.java_path, "auto (mojang runtime)", field_w, false);
        ui.end_row();
    });

    for f in &mut s.sets {
        share_warning(app, &s.id, f, ui);
    }

    ui.add_space(18.0);
    let mut out = EditOutcome::Keep;
    ui.horizontal(|ui| {
        let save = w::primary_button(ui, &t, "save").clicked() || activate || ui.input(|i| i.key_pressed(egui::Key::Enter));
        if w::button(ui, "cancel").clicked() {
            out = EditOutcome::Close;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if w::danger_button(ui, &t, "delete…").clicked() {
                out = EditOutcome::Delete;
            }
        });
        let new: [String; 2] = [0, 1].map(|n| s.sets[n].value.trim().to_string());
        let changed: Vec<Kind> = Kind::ALL.into_iter().zip(&new).filter(|(k, v)| app.instances.iter().any(|i| i.id == s.id && k.set_of(i) != v.as_str())).map(|(k, _)| k).collect();
        if save && let Err(e) = crate::launch::split_args(&s.jvm_args) {
            app.toast(ToastKind::Error, format!("JVM arguments: {e}"));
        } else if save && let Some(e) = set_change_error(app, s, &new, &changed) {
            app.toast(ToastKind::Error, e);
        } else if save {
            if let Some(i) = app.instances.iter_mut().find(|i| i.id == s.id) {
                if !s.name.trim().is_empty() {
                    i.name = s.name.trim().to_string();
                }
                i.group = s.group.trim().to_string();
                i.memory_mb = s.custom_memory.then_some(s.memory_mb);
                i.window = s.custom_window.then_some(s.window);
                i.jvm_args = s.jvm_args.trim().to_string();
                i.java_path = s.java_path.trim().to_string();
                [i.share_set, i.world_set] = new;
                if let Err(e) = i.save() {
                    app.toast(ToastKind::Error, format!("Save failed: {e}"));
                } else if !changed.is_empty() {
                    let i = i.clone();
                    app.start_sharing(ui.ctx(), vec![i]);
                }
            }
            out = EditOutcome::Close;
        }
    });
    out
}

/// Why the edit dialog can't save these set changes, if it can't.
fn set_change_error(app: &App, s: &EditState, new: &[String; 2], changed: &[Kind]) -> Option<String> {
    for &kind in changed {
        let set = &new[kind as usize];
        let old = &s.sets[kind as usize].old;
        if let Some(e) = (!set.is_empty()).then(|| crate::shares::check_set_name(set).err()).flatten() {
            return Some(format!("{e}"));
        }
        if let Some(why) = app.instance_busy(&s.id) {
            return Some(format!("Can't change the {} while {why}", kind.noun()));
        }
        if let Some(why) = [old, set].into_iter().find_map(|set| app.set_busy(kind, set, &s.id)) {
            return Some(format!("Can't change the {} while {why}", kind.noun()));
        }
        if kind == Kind::Worlds
            && let Some(inst) = app.instances.iter().find(|i| i.id == s.id)
            && let Some(why) = crate::world_sets::mismatch(inst, set, &app.instances)
        {
            return Some(format!("{why}. Change this instance's version to match first."));
        }
    }
    None
}

/// The instances in set `set` of this kind, and whether each is only still linked in
/// (it left on paper, but the leave hasn't finished).
pub fn set_members<'a>(kind: Kind, set: &'a str, instances: &'a [Instance], links: &'a std::collections::BTreeMap<String, Vec<String>>) -> impl Iterator<Item = (&'a Instance, bool)> {
    let linked = links.get(set);
    instances.iter().filter_map(move |i| match (!set.is_empty() && kind.set_of(i) == set, !set.is_empty() && linked.is_some_and(|l| l.contains(&i.id))) {
        (true, _) => Some((i, false)),
        (false, true) => Some((i, true)),
        (false, false) => None,
    })
}

/// What changing a set will do to the files, before saving.
fn share_warning(app: &App, id: &str, f: &mut SetField, ui: &mut Ui) {
    let t = &app.t;
    let new = f.value.trim();
    if new == f.old {
        return;
    }
    let old = f.old.clone();
    let what = match f.kind {
        Kind::Packs => "resource packs, shader packs and schematics",
        Kind::Worlds => "worlds",
    };
    let mismatch = (f.kind == Kind::Worlds).then(|| app.instances.iter().find(|i| i.id == id)).flatten().and_then(|i| crate::world_sets::mismatch(i, new, &app.instances));
    let (text, color) = if let Some(why) = mismatch {
        (format!("\u{f071} {why}."), t.red)
    } else if new.is_empty() {
        if f.leave_size.is_none() && f.leave_bytes.is_none() {
            let (kind, dir) = (f.kind, crate::shares::set_dir(f.kind, &old));
            f.leave_size = Some(crate::task::Promise::spawn(&app.rt, ui.ctx(), async move {
                Ok(tokio::task::spawn_blocking(move || kind.folders().iter().map(|f| crate::fsutil::tree_size(&dir.join(f))).sum()).await?)
            }));
        }
        if let Some(Ok(n)) = f.leave_size.as_ref().and_then(|p| p.take()) {
            f.leave_bytes = Some(n);
        }
        let size = f.leave_bytes.map(w::size_label).unwrap_or_else(|| "measuring…".into());
        (
            format!("\u{f071} Leaving “{old}” gives this instance its own copy of the set's {what} ({size}). Free on btrfs or xfs; on other filesystems it takes that much space again."),
            t.yellow,
        )
    } else if old.is_empty() {
        let more = match f.kind {
            Kind::Packs => "",
            Kind::Worlds => " Before each launch, worlds last played with mods this instance lacks are snapshotted first.",
        };
        (format!("This instance's {what} move into “{new}”, shared with its other instances. Name clashes keep both.{more}"), t.muted)
    } else {
        (format!("This instance's {what} stay with “{old}”; it uses “{new}”'s instead."), t.muted)
    };
    ui.add_space(10.0);
    w::para(ui, RichText::new(text).color(color));
}

impl App {
    /// Why the instance can't be deleted right now, if it can't: its game or
    /// downloads would carry on in a folder that no longer exists.
    pub fn instance_busy(&self, id: &str) -> Option<&'static str> {
        if self.is_running(id) {
            Some("it's running")
        } else if self.content.busy_with(id) {
            Some("content is installing")
        } else if self.exports.iter().any(|(.., i)| i == id) {
            Some("it's exporting")
        } else if self.worlds.busy_with(id) {
            Some("a world is being copied")
        } else if self.sharing.is_some() {
            Some("shared folders are moving")
        } else {
            None
        }
    }

    /// Why set `set`'s folders can't change right now: another instance in it is
    /// exporting them or installing into them, or (for worlds) duplicating or playing
    /// one, which a leave would copy halfway. `changing` is the instance asking.
    pub fn set_busy(&self, kind: Kind, set: &str, changing: &str) -> Option<String> {
        set_members(kind, set, &self.instances, self.set_links(kind)).find_map(|(i, _)| {
            if self.exports.iter().any(|(.., id)| *id == i.id) {
                Some(format!("{} is exporting", i.name))
            } else if self.content.busy_with(&i.id) {
                Some(format!("{} is installing content", i.name))
            } else if kind == Kind::Worlds && self.worlds.busy_with(&i.id) {
                Some(format!("{} is copying a world", i.name))
            } else if kind == Kind::Worlds && i.id != changing && self.is_running(&i.id) {
                Some(format!("{} is running", i.name))
            } else {
                None
            }
        })
    }

    pub fn confirm_delete_instance(&mut self, id: &str) {
        let name = self.instances.iter().find(|i| i.id == id).map(|i| i.name.clone()).unwrap_or_default();
        if let Some(why) = self.instance_busy(id) {
            self.toast(ToastKind::Error, format!("Can't delete {name} while {why}"));
            return;
        }
        self.modal = Some(Modal::Confirm(Confirm {
            title: "delete instance".into(),
            body: format!("Permanently delete “{name}” and all of its worlds, mods and settings, and their backups?"),
            button: "delete".into(),
            action: ConfirmAction::DeleteInstance(id.to_string()),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::Loader;

    #[test]
    fn set_members_count_instances_still_linked_in() {
        let inst = |id: &str, set: &str| {
            let mut i = Instance::new(id, "1.21.1", Loader::Vanilla);
            i.id = id.into();
            i.share_set = set.into();
            i
        };
        // b left "packs" on paper, but its leave hasn't finished: its folders still link in.
        let all = [inst("a", "packs"), inst("b", ""), inst("c", "other"), inst("d", "")];
        let links = [("packs".to_string(), vec!["a".to_string(), "b".to_string()])].into();
        let got: Vec<(&str, bool)> = set_members(Kind::Packs, "packs", &all, &links).map(|(i, leaving)| (i.id.as_str(), leaving)).collect();
        assert_eq!(got, [("a", false), ("b", true)]);
        // So an export or install in b holds up changes to the set, as one in a does.
        assert_eq!(set_members(Kind::Packs, "other", &all, &links).count(), 1);
        assert_eq!(set_members(Kind::Packs, "", &all, &links).count(), 0);
    }
}
