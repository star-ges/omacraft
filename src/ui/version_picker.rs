//! Minecraft version + mod loader chooser, shared by the new-instance dialog
//! and an instance's version tab.

use egui::{RichText, Ui};

use super::App;
use super::nav::Nav;
use super::widgets as w;
use crate::loaders::{self, Build, Loader, LoaderKind};
use crate::task::Promise;

pub struct VersionPicker {
    filter: String,
    snapshots: bool,
    pub version: Option<String>,
    pub kind: LoaderKind,
    loader_versions: Option<Result<Vec<Build>, String>>,
    loader_p: Option<(LoaderKind, String, Promise<Vec<Build>>)>,
    loader_version: Option<String>,
    /// Loader version to keep selected once the list arrives.
    prefer: Option<String>,
    pub focus_filter: bool,
    /// Scroll the current version into view on the first frame.
    scroll_once: bool,
}

impl VersionPicker {
    pub fn new(snapshots: bool, current: Option<(&str, &Loader)>) -> Self {
        Self {
            filter: String::new(),
            snapshots,
            version: current.map(|(v, _)| v.to_string()),
            kind: current.map(|(_, l)| l.kind()).unwrap_or(LoaderKind::Vanilla),
            loader_versions: None,
            loader_p: None,
            loader_version: None,
            prefer: current.and_then(|(_, l)| l.version().map(String::from)),
            focus_filter: false,
            scroll_once: current.is_some(),
        }
    }

    pub fn cycle_kind(&mut self) {
        let all = LoaderKind::ALL;
        let i = all.iter().position(|k| *k == self.kind).unwrap_or(0);
        self.kind = all[(i + 1) % all.len()];
    }

    /// While typing in the filter: ↑/↓ pick a version, alt+↑/↓ a loader
    /// version, alt+←/→ the loader type. For dialogs, where the filter
    /// field holds keyboard focus.
    pub fn typing_keys(&mut self, ui: &Ui) -> (Option<Nav>, Option<Nav>) {
        use egui::{Key, Modifiers};
        ui.input_mut(|i| {
            let mut versions = None;
            let mut loader = None;
            if i.consume_key(Modifiers::ALT, Key::ArrowDown) {
                loader = Some(Nav::Down);
            } else if i.consume_key(Modifiers::ALT, Key::ArrowUp) {
                loader = Some(Nav::Up);
            } else if i.consume_key(Modifiers::ALT, Key::ArrowRight) || i.consume_key(Modifiers::ALT, Key::ArrowLeft) {
                self.cycle_kind();
            } else if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                versions = Some(Nav::Down);
            } else if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                versions = Some(Nav::Up);
            } else if i.consume_key(Modifiers::NONE, Key::PageDown) {
                versions = Some(Nav::PageDown);
            } else if i.consume_key(Modifiers::NONE, Key::PageUp) {
                versions = Some(Nav::PageUp);
            }
            (versions, loader)
        })
    }

    /// The chosen (minecraft version, loader), once both are known.
    pub fn selection(&self) -> Option<(String, Loader)> {
        let loader = match self.kind {
            LoaderKind::Vanilla => Loader::Vanilla,
            k => k.with(self.loader_version.clone()?),
        };
        Some((self.version.clone()?, loader))
    }

    /// Default to the latest release and fetch loader versions as needed.
    pub fn poll(&mut self, app: &App, ctx: &egui::Context) {
        if self.version.is_none()
            && let Some(Ok(m)) = &app.manifest
        {
            self.version = Some(m.latest.release.clone());
        }
        let (Some(mc), true) = (&self.version, self.kind != LoaderKind::Vanilla) else { return };
        let stale = self.loader_p.as_ref().is_none_or(|(k, v, _)| *k != self.kind || v != mc);
        if stale {
            let (kind, mc2) = (self.kind, mc.clone());
            self.loader_versions = None;
            self.loader_version = None;
            self.loader_p = Some((self.kind, mc.clone(), Promise::spawn(&app.rt, ctx, async move { loaders::versions_for(kind, &mc2).await })));
        }
        if let Some((_, _, p)) = &self.loader_p
            && let Some(r) = p.take()
        {
            let r = r.map_err(|e| format!("{e:#}"));
            if let Ok(list) = &r {
                let keep = self.prefer.take().filter(|v| list.iter().any(|b| &b.version == v));
                self.loader_version = keep.or_else(|| list.iter().find(|b| b.preferred).or(list.first()).map(|b| b.version.clone()));
            }
            self.loader_versions = Some(r);
        }
    }

    pub fn versions_ui(&mut self, app: &mut App, ui: &mut Ui, nav: Option<Nav>, focused: bool) {
        let t = app.t.clone();
        w::section(ui, &t, "minecraft version");
        ui.horizontal(|ui| {
            w::text_input(ui, &mut self.filter, "filter", 200.0, std::mem::take(&mut self.focus_filter));
            ui.checkbox(&mut self.snapshots, "snapshots");
        });
        ui.add_space(4.0);
        match &app.manifest {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("loading versions…").color(t.muted));
                });
            }
            Some(Err(e)) => {
                w::para(ui, RichText::new(e).color(t.red));
                if w::button(ui, "retry").clicked() {
                    app.load_manifest(ui.ctx());
                }
            }
            Some(Ok(m)) => {
                let shown: Vec<_> = m
                    .versions
                    .iter()
                    .filter(|v| {
                        let current = self.version.as_deref() == Some(v.id.as_str());
                        let kind_ok = v.kind == "release" || (self.snapshots && v.kind == "snapshot") || current;
                        kind_ok && (self.filter.is_empty() || v.id.contains(self.filter.trim()))
                    })
                    .collect();
                if let Some(n) = nav.filter(|n| n.is_move()) {
                    let pos = shown.iter().position(|v| self.version.as_deref() == Some(v.id.as_str()));
                    if let Some(p) = n.step(shown.len(), pos) {
                        self.version = Some(shown[p].id.clone());
                        self.scroll_once = true;
                    }
                }
                egui::ScrollArea::vertical().id_salt("versions").max_height(ui.available_height() - 4.0).auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for v in shown {
                        let current = self.version.as_deref() == Some(v.id.as_str());
                        let right = if v.kind == "release" { v.release_time.format("%Y-%m-%d").to_string() } else { v.kind.clone() };
                        let r = w::menu_row(ui, &t, current, "", &v.id, &right);
                        if current && focused {
                            w::focus_mark(ui, &t, r.rect);
                        }
                        if current && std::mem::take(&mut self.scroll_once) {
                            r.scroll_to_me(Some(egui::Align::Center));
                        }
                        if r.clicked() {
                            self.version = Some(v.id.clone());
                        }
                    }
                });
            }
        }
    }

    pub fn loader_ui(&mut self, app: &App, ui: &mut Ui, list_height: f32, nav: Option<Nav>, focused: bool) {
        let t = &app.t;
        w::section(ui, t, "mod loader");
        let opts: Vec<(LoaderKind, &str)> = LoaderKind::ALL.iter().map(|k| (*k, k.label())).collect();
        w::segmented(ui, t, &mut self.kind, &opts);
        ui.add_space(6.0);
        if self.kind == LoaderKind::Vanilla {
            ui.label(RichText::new("plain minecraft, no mods").color(t.muted));
            return;
        }
        match &self.loader_versions {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("loading loader versions…").color(t.muted));
                });
            }
            Some(Err(e)) => {
                w::para(ui, RichText::new(e).color(t.red));
            }
            Some(Ok(list)) if list.is_empty() => {
                w::para(
                    ui,
                    RichText::new(format!("{} doesn't support {} yet", self.kind.label(), self.version.as_deref().unwrap_or("this version")))
                        .color(t.yellow),
                );
            }
            Some(Ok(list)) => {
                let mut scroll = false;
                if let Some(n) = nav.filter(|n| n.is_move()) {
                    let shown = &list[..list.len().min(60)];
                    let pos = shown.iter().position(|b| self.loader_version.as_deref() == Some(b.version.as_str()));
                    if let Some(p) = n.step(shown.len(), pos) {
                        self.loader_version = Some(shown[p].version.clone());
                        scroll = true;
                    }
                }
                egui::ScrollArea::vertical().id_salt("loader-versions").max_height(list_height.max(80.0)).auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for Build { version: v, tag, .. } in list.iter().take(60) {
                        let sel = self.loader_version.as_deref() == Some(v.as_str());
                        let r = w::menu_row(ui, t, sel, "", v, tag);
                        if sel && focused {
                            w::focus_mark(ui, t, r.rect);
                        }
                        if sel && scroll {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            self.loader_version = Some(v.clone());
                        }
                    }
                });
            }
        }
    }
}
