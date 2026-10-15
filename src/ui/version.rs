//! An instance's components (Minecraft + loader), and changing them.

use egui::{RichText, Ui};

use super::nav::{Nav, Zone};
use super::version_picker::VersionPicker;
use super::widgets as w;
use super::App;
use crate::instance::Instance;
use crate::loaders::Loader;

#[derive(Default)]
pub struct VersionState {
    /// Open picker, for this instance id.
    picker: Option<(String, VersionPicker)>,
}

impl VersionState {
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    pub fn close(&mut self) {
        self.picker = None;
    }

    pub fn cycle_loader(&mut self) {
        if let Some((_, p)) = &mut self.picker {
            p.cycle_kind();
        }
    }
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    if app.version.picker.as_ref().is_some_and(|(id, _)| *id != inst.id) {
        app.version.picker = None;
    }

    w::section(ui, &t, "components");
    w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("grass")), "Minecraft", &inst.mc_version, "");
    match &inst.loader {
        Loader::Vanilla => {}
        Loader::Fabric(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("fabricmc")), "Fabric Loader", v, "");
        }
        Loader::Quilt(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("quiltmc")), "Quilt Loader", v, "");
        }
        Loader::Forge(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("gear")), "Forge", v, "");
        }
        Loader::NeoForge(v) => {
            w::rich_row(ui, &t, false, w::Lead::Image(crate::icons::source("neoforged")), "NeoForge", v, "");
        }
    }
    ui.add_space(10.0);

    let main_nav = app.nav_for(Zone::Main);
    let Some((_, picker)) = &mut app.version.picker else {
        ui.horizontal(|ui| {
            let running = app.is_running(&inst.id);
            let r = ui.add_enabled_ui(!running, |ui| w::button(ui, "change version…")).inner;
            if app.focus == Zone::Main {
                w::focus_mark(ui, &t, r.rect);
            }
            if r.clicked() || (!running && main_nav == Some(Nav::Activate)) {
                let p = VersionPicker::new(app.cfg.show_snapshots, Some((&inst.mc_version, &inst.loader)));
                app.version.picker = Some((inst.id.clone(), p));
            }
            if running {
                ui.label(RichText::new("stop the game to change versions").color(t.muted));
            }
        });
        return;
    };

    let mut picker = std::mem::replace(picker, VersionPicker::new(false, None));
    picker.poll(app, ui.ctx());
    let side_nav = app.nav_for(Zone::Side);
    let apply_key = main_nav == Some(Nav::Activate) || side_nav == Some(Nav::Activate);
    let mut close = false;
    let sel = picker.selection();
    let changed = sel.as_ref().is_some_and(|(mc, l)| *mc != inst.mc_version || *l != inst.loader);
    ui.horizontal(|ui| {
        if (ui.add_enabled_ui(changed, |ui| w::primary_button(ui, &t, "apply")).inner.clicked() || (changed && apply_key))
            && let Some((mc, loader)) = sel.clone()
            && let Some(i) = app.instances.iter_mut().find(|i| i.id == inst.id)
        {
            let mc_changed = i.mc_version != mc;
            i.mc_version = mc.clone();
            i.loader = loader.clone();
            match i.save() {
                Ok(()) => {
                    let what = if loader == Loader::Vanilla { mc.clone() } else { format!("{mc} with {}", loader.display()) };
                    app.toasts.push(super::Toast::info(format!("{} now uses {what}", inst.name)));
                    if mc_changed && !inst.mods().is_empty() {
                        app.toasts.push(super::Toast::info("Mods are built for one Minecraft version; check yours still match."));
                    }
                }
                Err(e) => app.toasts.push(super::Toast::error(format!("Save failed: {e}"))),
            }
            close = true;
        }
        if w::button(ui, "cancel").clicked() {
            close = true;
        }
        if let Some((mc, loader)) = &sel
            && changed
        {
            let text = if *loader == Loader::Vanilla { mc.clone() } else { format!("{mc}  ·  {}", loader.display()) };
            ui.label(RichText::new(format!("→ {text}")).color(t.accent));
        }
    });
    ui.add_space(6.0);
    ui.columns(2, |cols| {
        let focus = app.focus;
        picker.versions_ui(app, &mut cols[0], main_nav, focus == Zone::Main);
        let h = cols[1].available_height() - 60.0;
        picker.loader_ui(app, &mut cols[1], h, side_nav, focus == Zone::Side);
    });
    if close {
        app.version.picker = None;
    } else if let Some((_, slot)) = &mut app.version.picker {
        *slot = picker;
    }
}
