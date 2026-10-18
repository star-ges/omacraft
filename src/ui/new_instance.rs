use std::path::PathBuf;
use std::time::Instant;

use egui::{RichText, Ui};

use super::nav::Nav;
use super::version_picker::VersionPicker;
use super::widgets as w;
use super::{App, ToastKind};
use crate::config::Config;
use crate::import::{self, Found};
use crate::instance::Instance;
use crate::modrinth::{self, Hit, SearchResponse, Version};
use crate::task::Promise;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Create,
    Import,
}

/// Where an import comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum From {
    Files,
    Modrinth,
}

/// Modrinth modpack search, inside the import dialog.
#[derive(Default)]
struct Packs {
    query: String,
    /// When the query last changed, so a search goes out once typing pauses.
    typed: Option<Instant>,
    /// The last query sent.
    searched: Option<String>,
    search_p: Option<Promise<SearchResponse>>,
    results: Option<Result<Vec<Hit>, String>>,
    pack: Option<Hit>,
    versions_p: Option<Promise<Vec<Version>>>,
    versions: Option<Result<Vec<Version>, String>>,
    /// The chosen version's id.
    version: Option<String>,
    scroll_pack: bool,
    scroll_version: bool,
    focus: bool,
}

impl Packs {
    fn search(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context) {
        let q = self.query.clone();
        self.searched = Some(q.clone());
        self.typed = None;
        self.search_p = Some(Promise::spawn(rt, ctx, async move { modrinth::search_packs(&q).await }));
    }

    fn pick(&mut self, hit: &Hit, rt: &tokio::runtime::Handle, ctx: &egui::Context) {
        if self.pack.as_ref().is_some_and(|p| p.project_id == hit.project_id) {
            return;
        }
        self.pack = Some(hit.clone());
        self.versions = None;
        self.version = None;
        let id = hit.project_id.clone();
        self.versions_p = Some(Promise::spawn(rt, ctx, async move { modrinth::pack_versions(&id).await }));
    }

    fn poll(&mut self) {
        if let Some(p) = &self.search_p
            && let Some(r) = p.take()
        {
            self.search_p = None;
            self.results = Some(r.map(|s| s.hits).map_err(|e| format!("{e:#}")));
        }
        if let Some(p) = &self.versions_p
            && let Some(r) = p.take()
        {
            self.versions_p = None;
            // Newest first, so the first one is the pack as it is now.
            self.version = r.as_ref().ok().and_then(|v| v.first()).map(|v| v.id.clone());
            self.versions = Some(r.map_err(|e| format!("{e:#}")));
        }
    }

    /// What the importer downloads: the chosen version's page, which it resolves to the
    /// `.mrpack` and its SHA-1.
    fn source(&self) -> Option<String> {
        let (pack, version) = (self.pack.as_ref()?, self.version.as_ref()?);
        Some(format!("https://modrinth.com/modpack/{}/version/{version}", pack.slug))
    }
}

pub struct NewInstance {
    pub mode: Mode,
    name: String,
    name_touched: bool,
    group: String,
    picker: VersionPicker,
    /// Instance to copy game settings from (id), and the ones that have any (id, name, version).
    settings_from: Option<String>,
    settings_sources: Option<Vec<(String, String, String)>>,
    // import
    pub source: String,
    found: Option<Vec<Found>>,
    browse_p: Option<Promise<Option<PathBuf>>>,
    focus_source: bool,
    from: From,
    packs: Packs,
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
            settings_from: None,
            settings_sources: None,
            source: String::new(),
            found: None,
            browse_p: None,
            focus_source: false,
            from: From::Files,
            packs: Packs::default(),
            cancelled: false,
        }
    }

    pub fn import(cfg: &Config, source: String) -> Self {
        let mut s = Self::new(cfg);
        s.focus_source = source.is_empty();
        s.set_source(source);
        s
    }

    /// Switch to importing from a file, folder or link (a dropped file).
    pub fn set_source(&mut self, source: String) {
        self.mode = Mode::Import;
        self.from = From::Files;
        self.source = source;
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
    group_field(ui, t, &mut s.group, &app.groups.names(), "no group", ui.available_width());
}

/// Text field with a dropdown of existing groups (or share sets); `none` clears it.
pub fn group_field(ui: &mut Ui, t: &crate::theme::Theme, value: &mut String, groups: &[String], none: &str, width: f32) {
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
                if ui.button(RichText::new(none).color(t.muted)).clicked() {
                    value.clear();
                    ui.close();
                }
            });
        }
    });
}

/// Picks an instance whose game settings (options.txt) the new one starts with.
fn settings_field(app: &App, s: &mut NewInstance, ui: &mut Ui) {
    let t = &app.t;
    let sources = s.settings_sources.get_or_insert_with(|| app.instances.iter().filter(|i| i.has_settings()).map(|i| (i.id.clone(), i.name.clone(), i.mc_version.clone())).collect());
    w::section(ui, t, "settings");
    let chosen = s.settings_from.as_ref().and_then(|id| sources.iter().find(|(i, _, _)| i == id));
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_enabled_ui(!sources.is_empty(), |ui| {
                ui.menu_button("\u{f44b}", |ui| {
                    for (id, name, mc) in sources.iter() {
                        if ui.button(format!("{name}  ·  {mc}")).clicked() {
                            s.settings_from = Some(id.clone());
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button(RichText::new("game defaults").color(t.muted)).clicked() {
                        s.settings_from = None;
                        ui.close();
                    }
                });
            });
            let text = match chosen {
                Some((_, name, _)) => RichText::new(format!("copy from {name}")),
                None if sources.is_empty() => RichText::new("game defaults (no instance has settings yet)").color(t.muted),
                None => RichText::new("game defaults").color(t.muted),
            };
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| ui.add(egui::Label::new(text).truncate()));
        });
    });
    // Keybinds from 1.13 on use key names, which older versions can't read (they fall back to defaults).
    let target = s.picker.version.as_deref().unwrap_or_default();
    let old = |v: &str| crate::world_sets::compare_versions(v, "1.13") == std::cmp::Ordering::Less;
    if let Some((_, _, mc)) = chosen
        && !target.is_empty()
        && old(target)
        && !old(mc)
    {
        ui.label(RichText::new("keybinds won't carry over to versions before 1.13").color(t.yellow));
    }
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
        settings_field(app, s, ui);
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
                        Ok(()) => {
                            let from = s.settings_from.as_ref().and_then(|id| app.instances.iter().find(|i| &i.id == id));
                            if let Some(from) = from
                                && let Err(e) = inst.copy_settings_from(from)
                            {
                                app.toast(ToastKind::Error, format!("Created {}, but couldn't copy settings from {}: {e}", inst.name, from.name));
                            }
                            out = Some(Outcome::Created(inst));
                        }
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
    s.packs.poll();
    let found = s.found.get_or_insert_with(import::detect).clone();
    let (pack_nav, version_nav) = if s.from == From::Modrinth { list_keys(ui) } else { (None, None) };
    let nav = app.modal_nav();
    let pack_nav = pack_nav.or(nav.filter(|n| n.is_move()));
    let mut out = None;
    let mut submit = nav == Some(Nav::Activate);
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        if w::segmented(ui, &t, &mut s.from, &[(From::Files, "files and launchers"), (From::Modrinth, "modrinth")]) {
            match s.from {
                From::Files => s.focus_source = true,
                From::Modrinth => s.packs.focus = true,
            }
        }
        ui.add_space(4.0);
        match s.from {
            From::Files => submit |= files_ui(app, s, ui, &found),
            From::Modrinth => submit |= packs_ui(app, s, ui, pack_nav),
        }

        let ui = &mut cols[1];
        let hint = match (&s.from, &s.packs.pack) {
            (From::Modrinth, Some(p)) => p.title.clone(),
            _ => "from the pack or instance".to_string(),
        };
        name_and_group(app, s, ui, &hint);
        ui.add_space(6.0);
        if s.from == From::Modrinth {
            versions_ui(app, s, ui, version_nav);
        } else {
            w::para(ui, RichText::new("Files are copied, so the original stays as it is. On btrfs the copy is instant and takes no extra space.").color(t.muted));
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            ui.horizontal(|ui| {
                let source = match s.from {
                    From::Files => Some(s.source.trim().to_string()).filter(|s| !s.is_empty()),
                    From::Modrinth => s.packs.source(),
                };
                let ready = source.is_some();
                let go = ui.add_enabled_ui(ready, |ui| w::primary_button(ui, &t, "import")).inner.clicked() || (ready && submit);
                if w::button(ui, "cancel").clicked() {
                    s.cancelled = true;
                }
                if go && let Some(source) = source {
                    let name = match (&s.from, &s.packs.pack) {
                        // The pack's own name is often its slug or carries a version; the title is what Modrinth shows.
                        (From::Modrinth, Some(p)) if !s.name_touched => Some(p.title.clone()),
                        _ => Some(s.name.trim().to_string()).filter(|n| s.name_touched && !n.is_empty()),
                    };
                    out = Some(Outcome::Import(import::Request { source, name, group: s.group.trim().to_string() }));
                }
            });
        });
    });
    out
}

/// Arrow keys move through packs, alt+arrows through the chosen pack's versions, also while
/// the search field has focus.
fn list_keys(ui: &Ui) -> (Option<Nav>, Option<Nav>) {
    use egui::{Key, Modifiers};
    ui.input_mut(|i| {
        if i.consume_key(Modifiers::ALT, Key::ArrowDown) {
            (None, Some(Nav::Down))
        } else if i.consume_key(Modifiers::ALT, Key::ArrowUp) {
            (None, Some(Nav::Up))
        } else if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
            (Some(Nav::Down), None)
        } else if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
            (Some(Nav::Up), None)
        } else if i.consume_key(Modifiers::NONE, Key::PageDown) {
            (Some(Nav::PageDown), None)
        } else if i.consume_key(Modifiers::NONE, Key::PageUp) {
            (Some(Nav::PageUp), None)
        } else {
            (None, None)
        }
    })
}

/// A path, link or another launcher's instance. Returns whether enter was pressed in the field.
fn files_ui(app: &App, s: &mut NewInstance, ui: &mut Ui, found: &[Found]) -> bool {
    let t = &app.t;
    w::section(ui, t, "from a file, folder or link");
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
    w::section(ui, t, "from other launchers");
    if found.is_empty() {
        ui.label(RichText::new("no Prism, PolyMC or MultiMC instances found").color(t.muted));
    }
    egui::ScrollArea::vertical().id_salt("found").max_height(ui.available_height() - 4.0).auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for f in found {
            let path = f.dir.display().to_string();
            let sel = s.source == path;
            let detail = match &f.unsupported {
                Some(why) => why.clone(),
                None => format!("{}  ·  {}  ·  {}", f.launcher, f.mc_version, f.loader),
            };
            let r = w::rich_row(ui, t, sel, w::Lead::Glyph(if f.unsupported.is_some() { "\u{f0159}" } else { "\u{f01a7}" }), &f.name, &detail, "");
            if r.clicked() && f.unsupported.is_none() {
                s.source = path;
            }
        }
    });
    submit
}

/// Modrinth modpack search. Returns whether enter was pressed with nothing new to search for.
fn packs_ui(app: &App, s: &mut NewInstance, ui: &mut Ui, nav: Option<Nav>) -> bool {
    let (t, ctx, rt) = (&app.t, ui.ctx().clone(), app.rt.clone());
    let p = &mut s.packs;
    if p.searched.is_none() && p.search_p.is_none() {
        p.search(&rt, &ctx);
    }
    let r = w::text_input(ui, &mut p.query, "search modrinth modpacks", ui.available_width(), std::mem::take(&mut p.focus));
    if r.changed() {
        p.typed = Some(Instant::now());
        ctx.request_repaint_after(super::content::SEARCH_DELAY);
    }
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let unsent = p.searched.as_ref() != Some(&p.query);
    if unsent && (enter || p.typed.is_some_and(|at| at.elapsed() >= super::content::SEARCH_DELAY)) {
        p.search(&rt, &ctx);
    }
    if p.search_p.is_some() && p.results.is_some() {
        let side = r.rect.height() - 10.0;
        let rect = egui::Rect::from_min_size(r.rect.right_top() + egui::vec2(-side - 5.0, 5.0), egui::vec2(side, side));
        egui::Spinner::new().color(t.muted).paint_at(ui, rect);
    }
    ui.add_space(4.0);
    let hits = match &p.results {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("searching…").color(t.muted));
            });
            return false;
        }
        Some(Err(e)) => {
            w::para(ui, RichText::new(e).color(t.red));
            if w::button(ui, "retry").clicked() {
                p.search(&rt, &ctx);
            }
            return false;
        }
        Some(Ok(h)) if h.is_empty() => {
            ui.label(RichText::new("no modpacks found").color(t.muted));
            return false;
        }
        Some(Ok(h)) => h.clone(),
    };
    let at = hits.iter().position(|h| p.pack.as_ref().is_some_and(|k| k.project_id == h.project_id));
    if let Some(n) = nav.and_then(|n| n.step(hits.len(), at)) {
        p.pick(&hits[n], &rt, &ctx);
        p.scroll_pack = true;
    }
    egui::ScrollArea::vertical().id_salt("packs").max_height(ui.available_height() - 4.0).auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for h in &hits {
            let sel = p.pack.as_ref().is_some_and(|k| k.project_id == h.project_id);
            let lead = match h.icon_url.as_deref().and_then(|u| app.content.remote.get(u, &rt, &ctx)) {
                Some(path) => w::Lead::Image(super::content::file_image(&path)),
                None => w::Lead::Glyph("\u{f03d7}"),
            };
            let detail = format!("by {}  ·  \u{f01da} {}", h.author, modrinth::format_count(h.downloads));
            let r = w::rich_row(ui, t, sel, lead, &h.title, &detail, "").on_hover_text(&h.description);
            if sel {
                w::focus_mark(ui, t, r.rect);
                if std::mem::take(&mut p.scroll_pack) {
                    r.scroll_to_me(None);
                }
            }
            if r.clicked() {
                p.pick(h, &rt, &ctx);
            }
        }
    });
    enter && !unsent
}

/// The chosen pack's versions, newest first and preselected.
fn versions_ui(app: &App, s: &mut NewInstance, ui: &mut Ui, nav: Option<Nav>) {
    let t = &app.t;
    let p = &mut s.packs;
    w::section(ui, t, "version");
    let Some(pack) = &p.pack else {
        ui.label(RichText::new("pick a modpack").color(t.muted));
        return;
    };
    let versions = match &p.versions {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("loading versions…").color(t.muted));
            });
            return;
        }
        Some(Err(e)) => {
            w::para(ui, RichText::new(e).color(t.red));
            return;
        }
        Some(Ok(v)) if v.is_empty() => {
            ui.label(RichText::new(format!("{} has no versions", pack.title)).color(t.muted));
            return;
        }
        Some(Ok(v)) => v,
    };
    let at = versions.iter().position(|v| p.version.as_ref() == Some(&v.id));
    if let Some(n) = nav.and_then(|n| n.step(versions.len(), at)) {
        p.version = Some(versions[n].id.clone());
        p.scroll_version = true;
    }
    let mut picked = None;
    egui::ScrollArea::vertical().id_salt("pack-versions").max_height(ui.available_height() - 50.0).auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for v in versions {
            let sel = p.version.as_ref() == Some(&v.id);
            let mut game = v.game_versions.join(", ");
            if let Some(l) = v.loaders.iter().find(|l| *l != "minecraft") {
                game = format!("{game}  ·  {l}");
            }
            if !v.version_type.is_empty() && v.version_type != "release" {
                game = format!("{}  ·  {game}", v.version_type);
            }
            let r = w::menu_row(ui, t, sel, "", &v.version_number, &game);
            if sel && std::mem::take(&mut p.scroll_version) {
                r.scroll_to_me(Some(egui::Align::Center));
            }
            if r.clicked() {
                picked = Some(v.id.clone());
            }
        }
    });
    if picked.is_some() {
        p.version = picked;
    }
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
