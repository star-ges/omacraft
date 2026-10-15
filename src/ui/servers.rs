//! The in-game multiplayer server list (`servers.dat`).

use std::time::{Duration, Instant};

use egui::{RichText, Ui};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, ToastKind};
use crate::instance::Instance;
use crate::launch::QuickPlay;
use crate::saves::{self, Server};

struct Editing {
    index: Option<usize>,
    name: String,
    ip: String,
    focus: bool,
}

#[derive(Default)]
pub struct ServersState {
    cache: Option<(String, Result<Vec<Server>, String>, Instant)>,
    selected: Option<usize>,
    editing: Option<Editing>,
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let running = app.is_running(&inst.id);
    let s = &mut app.servers;
    let fresh = s.cache.as_ref().is_some_and(|(id, _, at)| *id == inst.id && at.elapsed() < Duration::from_secs(5));
    if !fresh {
        if s.cache.as_ref().is_some_and(|(id, _, _)| *id != inst.id) {
            s.selected = None;
            s.editing = None;
        }
        s.cache = Some((inst.id.clone(), saves::read_servers(&inst.game_dir()).map_err(|e| format!("{e:#}")), Instant::now()));
    }
    let mut servers = match &s.cache.as_ref().unwrap().1 {
        Ok(v) => v.clone(),
        Err(e) => {
            w::para(ui, RichText::new(e).color(t.red));
            return;
        }
    };
    let kb = if s.editing.is_some() { None } else { app.nav_for(Zone::Main) };
    let focused = app.focus == Zone::Main;
    let s = &mut app.servers;
    let mut scroll = false;
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(servers.len(), s.selected)) {
        s.selected = Some(p);
        scroll = true;
    }
    let key = |n: Nav| kb == Some(n);
    let sel = s.selected.filter(|&i| i < servers.len());
    let mut changed = false;
    let mut join = None;

    ui.horizontal(|ui| {
        if let Some(i) = sel {
            if w::primary_button(ui, &t, "󰐊 join").clicked() || key(Nav::Activate) {
                join = Some(servers[i].ip.clone());
            }
            if w::button(ui, "edit").clicked() || key(Nav::Edit) || key(Nav::Rename) {
                s.editing = Some(Editing { index: Some(i), name: servers[i].name.clone(), ip: servers[i].ip.clone(), focus: true });
            }
            if i > 0 && (w::button(ui, "↑").on_hover_text("move up").clicked() || key(Nav::MoveUp)) {
                servers.swap(i, i - 1);
                s.selected = Some(i - 1);
                changed = true;
            }
            if i + 1 < servers.len() && (w::button(ui, "↓").on_hover_text("move down").clicked() || key(Nav::MoveDown)) {
                servers.swap(i, i + 1);
                s.selected = Some(i + 1);
                changed = true;
            }
            if w::danger_button(ui, &t, "remove").clicked() || key(Nav::Delete) {
                servers.remove(i);
                s.selected = None;
                changed = true;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if w::button(ui, "+ add server").clicked() || key(Nav::Add) {
                s.editing = Some(Editing { index: None, name: String::new(), ip: String::new(), focus: true });
            }
        });
    });
    if running {
        w::para(ui, RichText::new("The game is running and may overwrite changes made here.").color(t.yellow));
    }

    if let Some(e) = &mut s.editing {
        ui.add_space(6.0);
        let mut done = None;
        let focus = std::mem::take(&mut e.focus);
        ui.horizontal(|ui| {
            w::field_label(ui, &t, "name");
            w::text_input(ui, &mut e.name, "Minecraft Server", 280.0, focus);
        });
        ui.horizontal(|ui| {
            w::field_label(ui, &t, "address");
            let r = w::text_input(ui, &mut e.ip, "play.example.com", 280.0, false);
            let ok = !e.ip.trim().is_empty();
            if ui.add_enabled_ui(ok, |ui| w::primary_button(ui, &t, "save")).inner.clicked()
                || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                done = Some(true);
            }
            if w::button(ui, "cancel").clicked() {
                done = Some(false);
            }
        });
        if done == Some(true) {
            let name = if e.name.trim().is_empty() { "Minecraft Server" } else { e.name.trim() };
            match e.index {
                Some(i) if i < servers.len() => {
                    servers[i].name = name.to_string();
                    servers[i].ip = e.ip.trim().to_string();
                }
                _ => {
                    servers.push(Server::new(name, e.ip.trim()));
                    s.selected = Some(servers.len() - 1);
                }
            }
            changed = true;
        }
        if done.is_some() {
            s.editing = None;
        }
    }
    ui.add_space(6.0);

    if servers.is_empty() {
        w::empty_state(ui, &t, "󰒋", "no servers yet", "Add one here or from the multiplayer screen in game.");
    }
    let mut clicked = false;
    egui::ScrollArea::vertical().id_salt("servers").auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (i, srv) in servers.iter().enumerate() {
            let lead = match &srv.icon {
                Some(png) => w::Lead::Image(egui::ImageSource::Bytes {
                    uri: format!("bytes://server/{}/{}.png", inst.id, crate::net::short_hash(png)).into(),
                    bytes: png.clone().into(),
                }),
                None => w::Lead::Glyph("󰒋"),
            };
            let on = s.selected == Some(i);
            let r = w::rich_row(ui, &t, on, lead, &srv.name, &srv.ip, "");
            if on && focused {
                w::focus_mark(ui, &t, r.rect);
            }
            if on && scroll {
                r.scroll_to_me(None);
            }
            if r.clicked() {
                s.selected = Some(i);
                clicked = true;
            }
            if r.double_clicked() {
                join = Some(srv.ip.clone());
            }
        }
    });

    if clicked {
        app.focus = Zone::Main;
    }
    if changed {
        match saves::write_servers(&inst.game_dir(), &servers) {
            Ok(()) => s.cache = None,
            Err(e) => app.toast(ToastKind::Error, format!("Couldn't save servers: {e:#}")),
        }
    }
    if let Some(ip) = join {
        app.launch(ui.ctx(), &inst.id, Some(QuickPlay::Server(ip)));
    }
}
