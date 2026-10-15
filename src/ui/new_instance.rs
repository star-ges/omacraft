use std::path::PathBuf;

use egui::{RichText, Ui};

use super::nav::Nav;
use super::version_picker::VersionPicker;
use super::widgets as w;
use super::{App, ToastKind};
use crate::config::Config;
use crate::import::{self, Found};
use crate::instance::Instance;
use crate::task::Promise;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Create,
    Import,
}

pub struct NewInstance {
    pub mode: Mode,
    name: String,
    name_touched: bool,
    group: String,
    picker: VersionPicker,
    // import
    pub source: String,
    found: Option<Vec<Found>>,
    browse_p: Option<Promise<Option<PathBuf>>>,
    focus_source: bool,
    pub cancelled: bool,
}

/// What the dialog wants the app to do when it closes.
pub enum Outcome {
    Created(Instance),
    Import(import::Request),
}

impl NewInstance {
    pub fn new(cfg: &Config) -> Self {
        let mut picker = VersionPicker::new(cfg.show_snapshots, None);
        picker.focus_filter = true;
        Self {
            mode: Mode::Create,
            name: String::new(),
            name_touched: false,
            group: String::new(),
            picker,
            source: String::new(),
            found: None,
            browse_p: None,
            focus_source: false,
            cancelled: false,
        }
    }

    pub fn import(cfg: &Config, source: String) -> Self {
        let mut s = Self::new(cfg);
        s.mode = Mode::Import;
        s.focus_source = source.is_empty();
        s.source = source;
        s
    }

    fn default_name(&self) -> String {
        let v = self.picker.version.clone().unwrap_or_default();
        match self.picker.kind {
            crate::loaders::LoaderKind::Vanilla => v,
            k => format!("{v} {}", k.label()),
        }
    }
}

pub fn show(app: &mut App, s: &mut NewInstance, ui: &mut Ui) -> Option<Outcome> {
    let t = app.t.clone();
    let screen = ui.ctx().content_rect();
    ui.set_width((screen.width() - 120.0).clamp(480.0, 860.0));
    ui.set_height((screen.height() - 120.0).clamp(300.0, 560.0));

    ui.horizontal(|ui| {
        w::heading(ui, &t, "new instance");
        ui.add_space(16.0);
        if w::segmented(ui, &t, &mut s.mode, &[(Mode::Create, "new"), (Mode::Import, "import")]) && s.mode == Mode::Import {
            s.focus_source = true;
        }
    });
    ui.add_space(10.0);
    match s.mode {
        Mode::Create => create(app, s, ui),
        Mode::Import => import_ui(app, s, ui),
    }
}

/// Name + group fields, shared by both modes.
fn name_and_group(app: &App, s: &mut NewInstance, ui: &mut Ui, name_hint: &str) {
    let t = &app.t;
    w::section(ui, t, "name");
    if w::text_input(ui, &mut s.name, name_hint, ui.available_width(), false).changed() {
        s.name_touched = true;
    }
    ui.add_space(6.0);
    w::section(ui, t, "group");
    group_field(ui, t, &mut s.group, &app.groups.names(), ui.available_width());
}

/// Text field with a dropdown of existing groups.
pub fn group_field(ui: &mut Ui, t: &crate::theme::Theme, value: &mut String, groups: &[String], width: f32) {
    ui.horizontal(|ui| {
        let has = !groups.is_empty();
        w::text_input(ui, value, "none", width - if has { 40.0 } else { 0.0 }, false);
        if has {
            ui.menu_button("\u{f44b}", |ui| {
                for g in groups {
                    if ui.button(g).clicked() {
                        *value = g.clone();
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button(RichText::new("no group").color(t.muted)).clicked() {
                    value.clear();
                    ui.close();
                }
            });
        }
    });
}

fn create(app: &mut App, s: &mut NewInstance, ui: &mut Ui) -> Option<Outcome> {
    let t = app.t.clone();
    s.picker.poll(app, ui.ctx());
    let mut out = None;
    let (vnav, lnav) = s.picker.typing_keys(ui);
    let vnav = vnav.or(app.modal_nav());
    ui.columns(2, |cols| {
        s.picker.versions_ui(app, &mut cols[0], vnav, true);

        let ui = &mut cols[1];
        if !s.name_touched {
            s.name = s.default_name();
        }
        name_and_group(app, s, ui, "instance name");
        ui.add_space(6.0);
        let list_h = ui.available_height() - 110.0;
        s.picker.loader_ui(app, ui, list_h, lnav, false);

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            ui.horizontal(|ui| {
                let sel = s.picker.selection();
                let ready = sel.is_some() && !s.name.trim().is_empty();
                let create = ui.add_enabled_ui(ready, |ui| w::primary_button(ui, &t, "create")).inner.clicked()
                    || (ready && (vnav == Some(Nav::Activate) || ui.input(|i| i.key_pressed(egui::Key::Enter))));
                if w::button(ui, "cancel").clicked() {
                    s.cancelled = true;
                }
                if create && let Some((mc, loader)) = sel {
                    let mut inst = Instance::new(s.name.trim(), &mc, loader);
                    inst.group = s.group.trim().to_string();
                    match inst.assign_id().map_err(anyhow::Error::from).and_then(|()| inst.save()) {
                        Ok(()) => out = Some(Outcome::Created(inst)),
                        Err(e) => app.toast(ToastKind::Error, format!("Couldn't create instance: {e}")),
                    }
                }
            });
        });
    });
    out
}

fn import_ui(app: &mut App, s: &mut NewInstance, ui: &mut Ui) -> Option<Outcome> {
    let t = app.t.clone();
    if let Some(p) = &s.browse_p
        && let Some(r) = p.take()
    {
        s.browse_p = None;
        match r {
            Ok(Some(path)) => s.source = path.display().to_string(),
            Ok(None) => {}
            Err(e) => app.toast(ToastKind::Error, format!("File picker failed: {e:#}")),
        }
    }
    let found = s.found.get_or_insert_with(import::detect).clone();
    let mut out = None;
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        w::section(ui, &t, "from a file, folder or link");
        let r = w::text_input(ui, &mut s.source, "path or https://modrinth.com/modpack/…", ui.available_width(), std::mem::take(&mut s.focus_source));
        let submit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.horizontal(|ui| {
            let busy = s.browse_p.is_some();
            if ui.add_enabled_ui(!busy, |ui| w::button(ui, "browse file…")).inner.clicked() {
                s.browse_p = Some(pick(app, ui.ctx(), false));
            }
            if ui.add_enabled_ui(!busy, |ui| w::button(ui, "browse folder…")).inner.clicked() {
                s.browse_p = Some(pick(app, ui.ctx(), true));
            }
            if busy {
                ui.spinner();
            }
        });
        ui.add_space(4.0);
        w::para(
            ui,
            RichText::new("Prism or MultiMC exports (.zip), instance folders, Modrinth packs (.mrpack) or modrinth.com modpack links. You can also drop a file onto the window.")
                .color(t.muted),
        );

        ui.add_space(8.0);
        w::section(ui, &t, "from other launchers");
        if found.is_empty() {
            ui.label(RichText::new("no Prism, PolyMC or MultiMC instances found").color(t.muted));
        }
        egui::ScrollArea::vertical().id_salt("found").max_height(ui.available_height() - 4.0).auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for f in &found {
                let path = f.dir.display().to_string();
                let sel = s.source == path;
                let detail = match &f.unsupported {
                    Some(why) => why.clone(),
                    None => format!("{}  ·  {}  ·  {}", f.launcher, f.mc_version, f.loader),
                };
                let r = w::rich_row(ui, &t, sel, w::Lead::Glyph(if f.unsupported.is_some() { "󰅙" } else { "󰆧" }), &f.name, &detail, "");
                if r.clicked() && f.unsupported.is_none() {
                    s.source = path;
                }
            }
        });

        let ui = &mut cols[1];
        name_and_group(app, s, ui, "from the pack or instance");
        ui.add_space(6.0);
        w::para(ui, RichText::new("Files are copied, so the original stays as it is. On btrfs the copy is instant and takes no extra space.").color(t.muted));

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            ui.horizontal(|ui| {
                let ready = !s.source.trim().is_empty();
                let go = ui.add_enabled_ui(ready, |ui| w::primary_button(ui, &t, "import")).inner.clicked() || (ready && submit);
                if w::button(ui, "cancel").clicked() {
                    s.cancelled = true;
                }
                if go {
                    out = Some(Outcome::Import(import::Request {
                        source: s.source.trim().to_string(),
                        name: Some(s.name.trim().to_string()).filter(|n| s.name_touched && !n.is_empty()),
                        group: s.group.trim().to_string(),
                    }));
                }
            });
        });
    });
    out
}

/// Open the desktop file picker (via the XDG portal).
pub fn pick(app: &App, ctx: &egui::Context, folder: bool) -> Promise<Option<PathBuf>> {
    Promise::spawn(&app.rt, ctx, async move {
        let d = rfd::AsyncFileDialog::new().set_directory(dirs::download_dir().or_else(dirs::home_dir).unwrap_or_default());
        let picked = if folder {
            d.set_title("Import instance folder").pick_folder().await
        } else {
            d.set_title("Import instance or modpack").add_filter("Instances and modpacks", &["zip", "mrpack"]).pick_file().await
        };
        Ok(picked.map(|f| f.path().to_path_buf()))
    })
}
