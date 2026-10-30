use egui::{CornerRadius, RichText, Sense, Stroke, StrokeKind, Ui, vec2};

use super::widgets as w;
use super::{App, ToastKind};
use crate::keymap::{self, Action};
use crate::shares::Kind;
use crate::{paths, shares};

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 24, top: 16, bottom: 12 }).show(ui, |ui| {
            ui.set_max_width(720.0);
            w::heading(ui, &t, "settings");
            ui.add_space(8.0);
            let mut changed = false;

            w::section(ui, &t, "appearance");
            let theme = if !paths::omarchy_theme_dir().join("colors.toml").exists() {
                format!("{} (built in, Omarchy not found)", t.name)
            } else if t.name.is_empty() {
                "omarchy".to_string()
            } else {
                t.name.clone()
            };
            w::kv(ui, &t, "theme", theme);
            let font = app
                .fonts
                .regular
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "egui default".into());
            w::kv(ui, &t, "font", font);
            ui.horizontal(|ui| {
                ui.add_space(t.body_size() * 9.0 + 8.0);
                for c in [t.bg_darker, t.bg_dark, t.bg, t.bg_light, t.selection, t.muted, t.fg, t.accent, t.red, t.orange, t.yellow, t.green, t.cyan, t.blue, t.magenta] {
                    let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                    ui.painter().rect(r, CornerRadius::ZERO, c, Stroke::new(1.0, t.fg_a(0.15)), StrokeKind::Inside);
                }
            });
            w::para(ui, RichText::new("Colors and font follow `omarchy theme set` / `omarchy font set` live.").color(t.muted));

            ui.add_space(10.0);
            w::section(ui, &t, "java");
            ui.horizontal(|ui| {
                w::field_label(ui, &t, "memory");
                changed |= ui
                    .add(egui::Slider::new(&mut app.cfg.default_memory_mb, 1024..=32768).step_by(512.0).suffix(" MiB"))
                    .changed();
            });
            ui.horizontal(|ui| {
                w::field_label(ui, &t, "java");
                changed |= w::text_input(ui, &mut app.cfg.java_override, "auto (mojang runtime per version)", 420.0, false).changed();
            });
            ui.horizontal(|ui| {
                w::field_label(ui, &t, "jvm args");
                changed |= w::text_input(ui, &mut app.cfg.global_jvm_args, "applied to every instance", 420.0, false).changed();
            });
            ui.horizontal(|ui| {
                w::field_label(ui, &t, "wrapper");
                changed |= w::text_input(ui, &mut app.cfg.wrapper_command, "none (e.g. prime-run, gamemoderun)", 420.0, false).changed();
            });

            ui.add_space(10.0);
            w::section(ui, &t, "versions");
            changed |= ui.checkbox(&mut app.cfg.show_snapshots, "show snapshots by default").changed();

            ui.add_space(10.0);
            w::section(ui, &t, "microsoft sign-in");
            ui.horizontal(|ui| {
                w::field_label(ui, &t, "client id");
                changed |= w::text_input(ui, &mut app.cfg.msa_client_id, "use default", 420.0, false).changed();
            });
            egui::Frame::new().inner_margin(egui::Margin { left: (t.body_size() * 9.0 + 8.0) as i8, ..Default::default() }).show(ui, |ui| {
                w::para(
                    ui,
                    RichText::new("Leave this empty if Microsoft sign-in already works. It's only for using your own Azure app instead of Omacraft's.")
                        .color(t.muted),
                );
            });

            ui.add_space(10.0);
            shared(app, ui);

            ui.add_space(10.0);
            keys(app, ui);

            ui.add_space(10.0);
            w::section(ui, &t, "storage");
            w::kv(ui, &t, "data", paths::data_dir().display().to_string());
            w::kv(ui, &t, "config", paths::config_file().display().to_string());
            ui.horizontal(|ui| {
                ui.add_space(t.body_size() * 9.0 + 8.0);
                if w::button(ui, "open data folder").clicked() {
                    super::open_path(&paths::data_dir());
                }
                let running = app.reclaim.is_some();
                if ui.add_enabled(!running, egui::Button::new(if running { "reclaiming…" } else { "reclaim disk space" })).clicked() {
                    app.start_reclaim(ui.ctx());
                }
            });
            egui::Frame::new().inner_margin(egui::Margin { left: (t.body_size() * 9.0 + 8.0) as i8, ..Default::default() }).show(ui, |ui| {
                w::para(
                    ui,
                    RichText::new(
                        "Mods and packs are downloaded once and shared between instances on btrfs or xfs. Reclaim merges identical files that came in another way (imports, jars added by hand) and clears cached downloads no instance uses.",
                    )
                    .color(t.muted),
                );
            });

            ui.add_space(10.0);
            w::section(ui, &t, "backups");
            ui.horizontal(|ui| {
                w::field_label(ui, &t, "keep");
                changed |= ui.add(egui::Slider::new(&mut app.cfg.backups_keep, 0..=30).suffix(" per world")).changed();
                if w::button(ui, "open backups folder").clicked() {
                    let _ = std::fs::create_dir_all(crate::backups::root());
                    super::open_path(&crate::backups::root());
                }
            });
            egui::Frame::new().inner_margin(egui::Margin { left: (t.body_size() * 9.0 + 8.0) as i8, ..Default::default() }).show(ui, |ui| {
                w::para(
                    ui,
                    RichText::new(
                        "A world is backed up whenever a game that played it exits, keeping this many (0 turns that off). Mods and config are backed up before every update, keeping as many. Backups taken by hand or before a world is played with older mods are never removed. On btrfs or xfs backups share their bytes with the world.",
                    )
                    .color(t.muted),
                );
            });

            if changed && let Err(e) = app.cfg.save() {
                app.toast(ToastKind::Error, format!("Couldn't save settings: {e}"));
            }
        });
    });
}

/// The share sets and world sets with their members.
fn shared(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    w::section(ui, &t, "shared folders");
    let busy = app.sharing.is_some();
    for kind in Kind::ALL {
        let (title, glyph, what) = match kind {
            Kind::Packs => ("share sets", "\u{f0c1}", "resource packs, shader packs and schematics"),
            Kind::Worlds => ("world sets", "\u{f0ac}", "worlds; one minecraft version and loader per set"),
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).color(t.fg));
            ui.add(egui::Label::new(RichText::new(format!("·  {what}")).color(t.muted)).truncate());
        });
        ui.add_space(2.0);
        let sets = app.all_sets(kind);
        if sets.is_empty() {
            ui.label(RichText::new("none yet: edit an instance to pick one").color(t.muted));
        }
        for set in &sets {
            // Instances that left on paper but still link in (a leave that failed) count too.
            let members: Vec<String> = super::instances::set_members(kind, set, &app.instances, app.set_links(kind))
                .map(|(i, leaving)| if leaving { format!("{} (leaving)", i.name) } else { i.name.clone() })
                .collect();
            ui.horizontal(|ui| {
                // Buttons first, right to left, so the folder button keeps its place.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if w::button(ui, "folder").clicked() {
                        super::open_path(&shares::set_dir(kind, set));
                    }
                    if members.is_empty() && ui.add_enabled(!busy, egui::Button::new(RichText::new("delete…").color(t.red))).clicked() {
                        let what = match kind {
                            Kind::Packs => "the packs and schematics",
                            Kind::Worlds => "the worlds",
                        };
                        app.modal = Some(super::Modal::Confirm(super::Confirm {
                            title: format!("delete {}", kind.noun()),
                            body: format!("No instance uses “{set}”. Permanently delete it and {what} in it?"),
                            button: "delete".into(),
                            action: super::ConfirmAction::DeleteSet(kind, set.clone()),
                        }));
                    }
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.add(egui::Label::new(RichText::new(format!("{glyph}  {set}")).color(t.fg_light)).truncate());
                        let who = if members.is_empty() { "no instances".to_string() } else { members.join("  ·  ") };
                        ui.add(egui::Label::new(RichText::new(who).color(t.muted)).truncate());
                    });
                });
            });
            ui.add_space(4.0);
        }
    }
    ui.add_space(4.0);
    w::para(
        ui,
        RichText::new("Edit an instance to pick its sets. Joining moves the instance's files in, keeping both on a name clash; leaving gives it its own copy.")
            .color(t.muted),
    );
}

/// Rebinding: click a key to replace it, + to add one, right-click to remove.
fn keys(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    w::section(ui, &t, "keys");
    w::para(
        ui,
        RichText::new("Click a key to change it, + to add another, right-click a key to remove it. Esc cancels. Saved under [keys] in config.toml.").color(t.muted),
    );
    let mut section = "";
    let mut set: Option<(Action, Vec<crate::keymap::Chord>)> = None;
    for info in keymap::ACTIONS {
        if info.section != section {
            section = info.section;
            ui.add_space(6.0);
            ui.label(RichText::new(section).color(t.muted).size(t.body_size() - 1.0));
        }
        let chords = app.keymap.chords(info.action).to_vec();
        ui.horizontal(|ui| {
            let w_label = t.body_size() * 22.0;
            ui.allocate_ui_with_layout(egui::vec2(w_label, t.body_size() + 10.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_width(w_label);
                ui.add(egui::Label::new(RichText::new(info.what).color(t.fg)).truncate());
            });
            for (n, c) in chords.iter().enumerate() {
                let capturing = app.capture == Some((info.action, Some(n)));
                let clash = app.keymap.conflicts(info.action, c);
                let text = if capturing { "press a key…".to_string() } else { c.label() };
                let color = if capturing { t.accent } else if clash.is_empty() { t.fg } else { t.yellow };
                let mut b = egui::Button::new(RichText::new(text).color(color));
                if capturing {
                    b = b.stroke(Stroke::new(1.0, t.accent));
                }
                let hover = if clash.is_empty() {
                    "click to change, right-click to remove".to_string()
                } else {
                    let names: Vec<&str> = clash.iter().map(|a| keymap::info(*a).what).collect();
                    format!("also bound to: {}", names.join(", "))
                };
                let r = ui.add(b).on_hover_text(hover).on_hover_cursor(egui::CursorIcon::PointingHand);
                if r.clicked() {
                    app.capture = Some((info.action, Some(n)));
                }
                if r.secondary_clicked() {
                    let mut rest = chords.clone();
                    rest.remove(n);
                    set = Some((info.action, rest));
                }
            }
            let adding = app.capture == Some((info.action, None));
            let add = egui::Button::new(RichText::new(if adding { "press a key…" } else { "+" }).color(if adding { t.accent } else { t.muted }));
            if ui.add(add).on_hover_text("add another key").clicked() {
                app.capture = Some((info.action, None));
            }
            let defaults: Vec<_> = info.defaults.iter().filter_map(|d| keymap::Chord::parse(d)).collect();
            if chords != defaults && w::button(ui, "reset").clicked() {
                set = Some((info.action, defaults));
            }
        });
    }
    if let Some((a, chords)) = set {
        app.set_keys(a, chords);
    }
    ui.add_space(8.0);
    if !app.cfg.keys.is_empty() && w::danger_button(ui, &t, "reset all keys").clicked() {
        app.cfg.keys.clear();
        app.keymap = keymap::Keymap::new(&app.cfg.keys);
        if let Err(e) = app.cfg.save() {
            app.toast(ToastKind::Error, format!("Couldn't save keys: {e}"));
        }
    }
}
