use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::{RichText, Stroke, Ui};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::skin_view::{self, Model};
use super::{App, Confirm, ConfirmAction, Modal, Toast, ToastKind};
use crate::auth::{self, Account, Accounts, DeviceCode, MsaAccount};
use crate::skins::{self, Change, Loaded, Look, Profile};
use crate::task::Promise;

#[derive(Default)]
pub struct AccountsState {
    begin_p: Option<Promise<DeviceCode>>,
    code: Option<DeviceCode>,
    finish_p: Option<Promise<MsaAccount>>,
    offline_name: String,
    selected: Option<String>,
    /// Set by the `m` key.
    pub start_microsoft: bool,
    /// Set by the upload-skin key.
    pub start_upload: bool,
    /// Skins by account UUID.
    skins: HashMap<String, SkinSlot>,
    /// A skin file chosen but not uploaded yet, shown in the preview.
    pending: Option<PendingSkin>,
    pick_p: Option<Promise<Option<PathBuf>>>,
    /// The preview's model and the look it was built from.
    model: Option<(String, Model)>,
    /// The preview's (yaw, pitch) away from its starting view.
    turn: (f32, f32),
}

#[derive(Default)]
struct SkinSlot {
    shown: Option<Result<Shown, String>>,
    /// A load or change in flight, and whether it's an upload.
    busy: Option<(Promise<Loaded>, bool)>,
}

struct Shown {
    profile: Option<Profile>,
    look: Look,
}

struct PendingSkin {
    uuid: String,
    path: PathBuf,
    png: Vec<u8>,
    img: image::RgbaImage,
    slim: bool,
}

impl AccountsState {
    pub fn poll(&mut self, accounts: &mut Accounts, toasts: &mut Vec<Toast>) {
        let mut toast = |kind, text: String| toasts.push(Toast { kind, text, born: Instant::now() });
        if let Some(p) = &self.begin_p
            && let Some(r) = p.take()
        {
            self.begin_p = None;
            match r {
                Ok(code) => self.code = Some(code),
                Err(e) => toast(ToastKind::Error, format!("{e:#}")),
            }
        }
        if let Some(p) = &self.finish_p
            && let Some(r) = p.take()
        {
            self.finish_p = None;
            self.code = None;
            match r {
                Ok(acct) => {
                    let name = acct.name.clone();
                    let uuid = acct.uuid.clone();
                    // A fresh sign-in may come with a different skin.
                    self.skins.remove(&uuid);
                    accounts.upsert(Account::Msa(acct));
                    accounts.active = Some(uuid);
                    let _ = accounts.save();
                    toast(ToastKind::Info, format!("Signed in as {name}"));
                }
                Err(e) => toast(ToastKind::Error, format!("{e:#}")),
            }
        }
        for (uuid, slot) in &mut self.skins {
            let Some((p, upload)) = &slot.busy else { continue };
            let Some(r) = p.take() else { continue };
            let upload = *upload;
            slot.busy = None;
            match r {
                Ok(l) => {
                    if let Some(a) = l.account
                        && accounts.refresh(Account::Msa(a))
                    {
                        let _ = accounts.save();
                    }
                    slot.shown = Some(Ok(Shown { profile: l.profile, look: l.look }));
                    if upload && self.pending.as_ref().is_some_and(|p| &p.uuid == uuid) {
                        self.pending = None;
                        toast(ToastKind::Info, "Skin uploaded".into());
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if slot.shown.is_none() {
                        slot.shown = Some(Err(msg.clone()));
                    }
                    toast(ToastKind::Error, msg);
                }
            }
        }
        if let Some(p) = &self.pick_p
            && let Some(r) = p.take()
        {
            self.pick_p = None;
            if let Ok(Some(path)) = r
                && let Some(uuid) = self.selected.clone().or(accounts.active.clone())
                && let Err(e) = self.preview_file(&uuid, &path)
            {
                toast(ToastKind::Error, format!("{e:#}"));
            }
        }
    }

    pub fn turn_by(&mut self, steps: f32) {
        self.turn.0 += steps * 15f32.to_radians();
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }

    /// Show a skin file in the preview, ready to upload.
    fn preview_file(&mut self, uuid: &str, path: &Path) -> anyhow::Result<()> {
        let (png, img) = skins::read_skin_file(path)?;
        let slim = match self.skins.get(uuid).and_then(|s| s.shown.as_ref()) {
            Some(Ok(s)) => s.look.slim,
            _ => false,
        };
        self.pending = Some(PendingSkin { uuid: uuid.to_string(), path: path.to_path_buf(), png, img, slim });
        Ok(())
    }
}

/// The account whose skin is shown: the selected one, else the active one.
fn current(app: &App) -> Option<Account> {
    let find = |u: &str| app.accounts.accounts.iter().find(|a| a.uuid() == u).cloned();
    app.acct.selected.as_deref().and_then(find).or_else(|| app.accounts.active().cloned())
}

pub fn current_is_microsoft(app: &App) -> bool {
    matches!(current(app), Some(Account::Msa(_)))
}

/// Load an account's skin, or change it.
pub fn skin_change(app: &mut App, ctx: &egui::Context, uuid: &str, change: Change) {
    let Some(acct) = app.accounts.accounts.iter().find(|a| a.uuid() == uuid).cloned() else { return };
    let upload = matches!(change, Change::Upload { .. });
    let p = match acct {
        Account::Msa(a) => {
            let cid = app.cfg.client_id();
            Promise::spawn(&app.rt, ctx, async move { skins::apply(&cid, &a, change).await })
        }
        Account::Offline { uuid, .. } => Promise::spawn(&app.rt, ctx, async move { skins::default_look(&uuid).await }),
    };
    app.acct.skins.entry(uuid.to_string()).or_default().busy = Some((p, upload));
}

/// A PNG dropped on the accounts page becomes the pending skin.
pub fn drop_file(app: &mut App, path: &Path) -> bool {
    let Some(Account::Msa(a)) = current(app) else { return false };
    if let Err(e) = app.acct.preview_file(&a.uuid, path) {
        app.toast(ToastKind::Error, format!("{e:#}"));
    }
    true
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let kb = app.nav_for(Zone::Main);
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 24, top: 16, bottom: 12 }).show(ui, |ui| {
            w::heading(ui, &t, "accounts");
            ui.add_space(8.0);
            if ui.available_width() >= 1000.0 {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(600.0);
                        account_list(app, ui, kb);
                    });
                    ui.add_space(32.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(420.0);
                        skin_pane(app, ui, kb);
                    });
                });
            } else {
                ui.set_max_width(680.0);
                account_list(app, ui, kb);
                ui.add_space(14.0);
                skin_pane(app, ui, kb);
            }
        });
    });
}

fn account_list(app: &mut App, ui: &mut Ui, kb: Option<Nav>) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    w::section(ui, &t, "signed in");
    if app.accounts.accounts.is_empty() {
        ui.label(RichText::new("no accounts yet").color(t.muted));
    }
    let active = app.accounts.active.clone();
    let mut set_active = None;
    let uuids: Vec<String> = app.accounts.accounts.iter().map(|a| a.uuid().to_string()).collect();
    let current = app.acct.selected.clone().or(active.clone());
    let pos = current.as_ref().and_then(|u| uuids.iter().position(|x| x == u));
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(uuids.len(), pos)) {
        app.acct.selected = Some(uuids[p].clone());
    }
    // ⏎ uploads a pending skin rather than switching accounts.
    let pending = app.acct.pending.is_some();
    let key = |n: Nav| kb == Some(n) && !(n == Nav::Activate && pending);
    let focus_offline = key(Nav::Add);
    for a in &app.accounts.accounts {
        let is_active = active.as_deref() == Some(a.uuid());
        let sel = app.acct.selected.as_deref() == Some(a.uuid());
        let right = if is_active { format!("{}  ·  active", a.kind()) } else { a.kind().to_string() };
        let shown = sel || (app.acct.selected.is_none() && is_active);
        let r = w::menu_row(ui, &t, shown, "󰀄", a.name(), &right);
        if shown {
            w::focus_mark(ui, &t, r.rect);
        }
        if r.clicked() {
            app.acct.selected = Some(a.uuid().to_string());
        }
        if r.double_clicked() {
            set_active = Some(a.uuid().to_string());
        }
    }
    if let Some(sel) = app.acct.selected.clone().or(current.filter(|_| key(Nav::Delete))) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let other = active.as_deref() != Some(sel.as_str());
            if other && (w::button(ui, "make active").clicked() || key(Nav::Activate)) {
                set_active = Some(sel.clone());
            }
            if w::danger_button(ui, &t, "remove").clicked() || key(Nav::Delete) {
                app.accounts.remove(&sel);
                app.acct.selected = None;
                let _ = app.accounts.save();
            }
        });
    }
    if let Some(uuid) = set_active {
        app.accounts.active = Some(uuid);
        let _ = app.accounts.save();
    }

    ui.add_space(14.0);
    w::section(ui, &t, "microsoft");
    let client_id = app.cfg.client_id();
    if let Some(code) = app.acct.code.clone() {
        egui::Frame::new()
            .stroke(Stroke::new(2.0, t.border_active))
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("enter this code at").color(t.muted));
                ui.label(RichText::new(&code.verification_uri).color(t.accent));
                ui.add_space(6.0);
                ui.label(RichText::new(&code.user_code).font(crate::theme::bold(t.body_size() * 2.2)).color(t.fg_light));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if w::primary_button(ui, &t, "copy code & open browser").clicked() {
                        ui.ctx().copy_text(code.user_code.clone());
                        super::open_url(&code.verification_uri);
                    }
                    ui.spinner();
                    ui.label(RichText::new("waiting for sign-in…").color(t.muted));
                });
            });
    } else if app.acct.begin_p.is_some() {
        ui.spinner();
    } else if w::primary_button(ui, &t, "sign in with microsoft").clicked() || std::mem::take(&mut app.acct.start_microsoft) {
        let cid = client_id.clone();
        app.acct.begin_p = Some(Promise::spawn(&app.rt, &ctx, async move { auth::begin_device_code(&cid).await }));
    }
    // Once we have a code, start polling for the token.
    if let Some(code) = app.acct.code.clone()
        && app.acct.finish_p.is_none()
    {
        let cid = client_id.clone();
        app.acct.finish_p = Some(Promise::spawn(&app.rt, &ctx, async move { auth::finish_device_code(&cid, code).await }));
    }

    ui.add_space(14.0);
    w::section(ui, &t, "offline");
    w::para(ui, RichText::new("For singleplayer and offline-mode servers.").color(t.muted));
    ui.horizontal(|ui| {
        let r = w::text_input(ui, &mut app.acct.offline_name, "username", 220.0, focus_offline);
        let name = app.acct.offline_name.trim().to_string();
        let valid = (3..=16).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let submit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.add_enabled_ui(valid, |ui| w::button(ui, "add offline")).inner.clicked() || (submit && valid) {
            let acct = Account::offline(&name);
            app.accounts.active = Some(acct.uuid().to_string());
            app.accounts.upsert(acct);
            let _ = app.accounts.save();
            app.acct.offline_name.clear();
            app.toast(ToastKind::Info, format!("Added offline account {name}"));
        }
    });
}

fn skin_pane(app: &mut App, ui: &mut Ui, kb: Option<Nav>) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let key = |n: Nav| kb == Some(n);
    let Some(acct) = current(app) else { return };
    let uuid = acct.uuid().to_string();
    let msa = matches!(acct, Account::Msa(_));
    w::section(ui, &t, &format!("skin  ·  {}", acct.name()));

    let slot = app.acct.skins.entry(uuid.clone()).or_default();
    if slot.shown.is_none() && slot.busy.is_none() {
        skin_change(app, &ctx, &uuid, Change::None);
    }
    let slot = &app.acct.skins[&uuid];
    let busy = slot.busy.is_some();
    let shown = match &slot.shown {
        Some(Ok(s)) => Some(s),
        _ => None,
    };
    let error = match &slot.shown {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let profile = shown.and_then(|s| s.profile.clone());
    let pending = app.acct.pending.as_ref().filter(|p| p.uuid == uuid);

    // What the preview draws: a pending file, else the account's skin.
    let look = match (pending, shown) {
        (Some(p), s) => Some((format!("local:{}:{}", p.path.display(), p.slim), &p.img, p.slim, s.and_then(|s| s.look.cape.as_ref()))),
        (None, Some(s)) => Some((s.look.key.clone(), &s.look.skin, s.look.slim, s.look.cape.as_ref())),
        (None, None) => None,
    };
    let slim_now = look.as_ref().map(|l| l.2);
    match look {
        Some((k, img, slim, cape)) if app.acct.model.as_ref().is_none_or(|(m, _)| *m != k) => {
            let model = Model::build(img, slim, cape);
            app.acct.model = Some((k, model));
        }
        None => app.acct.model = None,
        _ => {}
    }

    let width = ui.available_width().min(320.0);
    if let Some(e) = &error {
        w::para(ui, RichText::new(format!("Couldn't load the skin: {e}")).color(t.red));
        if w::button(ui, "retry").clicked() {
            skin_change(app, &ctx, &uuid, Change::None);
        }
        ui.add_space(6.0);
    }
    if error.is_none() || app.acct.model.is_some() {
        skin_view::show(ui, &t, app.acct.model.as_ref().map(|(_, m)| m), &mut app.acct.turn, egui::vec2(width, width * 1.25));
        let k = &app.keymap;
        ui.label(
            RichText::new(format!("drag or {} to turn  ·  double-click to reset", k.pair(crate::keymap::Action::TurnLeft, crate::keymap::Action::TurnRight)))
                .color(t.muted)
                .size(t.body_size() - 1.0),
        );
    }
    ui.add_space(8.0);

    if !msa {
        w::para(
            ui,
            RichText::new("Offline accounts wear the default skin for their name (Steve or Alex). Changing skins needs a Microsoft account.")
                .color(t.muted),
        );
        return;
    }

    // The arm model: switches a pending file, or re-sends the current skin.
    if let Some(mut slim) = slim_now {
        ui.horizontal(|ui| {
            w::field_label(ui, &t, "model");
            let r = ui.add_enabled_ui(!busy, |ui| w::segmented(ui, &t, &mut slim, &[(false, "classic"), (true, "slim")])).inner;
            if (r || key(Nav::Toggle)) && !busy {
                let slim = if r { slim } else { !slim };
                if let Some(p) = app.acct.pending.as_mut().filter(|p| p.uuid == uuid) {
                    p.slim = slim;
                } else if let Some(s) = profile.as_ref().and_then(|p| p.skin()) {
                    skin_change(app, &ctx, &uuid, Change::Variant { url: s.url.clone(), slim });
                }
            }
        });
    }
    ui.add_space(6.0);

    if let Some((path, slim)) = app.acct.pending.as_ref().filter(|p| p.uuid == uuid).map(|p| (p.path.clone(), p.slim)) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        w::para(ui, RichText::new(format!("Previewing {name}. Upload it to {}?", acct.name())).color(t.fg));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let upload = ui.add_enabled_ui(!busy, |ui| w::button(ui, "upload skin")).inner.clicked() || (key(Nav::Activate) && !busy);
            if upload && let Some(png) = app.acct.pending.as_ref().map(|p| p.png.clone()) {
                skin_change(app, &ctx, &uuid, Change::Upload { png, slim });
            }
            if w::button(ui, "cancel").clicked() {
                app.acct.pending = None;
            }
            if busy {
                ui.spinner();
            }
        });
    } else {
        ui.horizontal(|ui| {
            let picking = app.acct.pick_p.is_some();
            let open = std::mem::take(&mut app.acct.start_upload);
            if ui.add_enabled_ui(!busy && !picking, |ui| w::button(ui, "upload skin…")).inner.clicked() || (open && !busy && !picking) {
                app.acct.pick_p = Some(Promise::spawn(&app.rt, &ctx, async {
                    let f = rfd::AsyncFileDialog::new()
                        .set_title("Choose a skin (64×64 PNG)")
                        .add_filter("PNG", &["png"])
                        .set_directory(dirs::download_dir().or_else(dirs::home_dir).unwrap_or_default())
                        .pick_file()
                        .await;
                    Ok(f.map(|f| f.path().to_path_buf()))
                }));
            }
            let custom = profile.as_ref().and_then(|p| p.skin()).is_some();
            if custom && ui.add_enabled_ui(!busy, |ui| w::danger_button(ui, &t, "reset")).inner.clicked() {
                app.modal = Some(Modal::Confirm(Confirm {
                    title: "reset skin".into(),
                    body: format!("Put {} back in the default skin? The current one isn't kept anywhere.", acct.name()),
                    button: "reset".into(),
                    action: ConfirmAction::ResetSkin(uuid.clone()),
                }));
            }
            if busy || picking {
                ui.spinner();
            }
        });
    }

    // Capes: none, or one of those the account owns.
    let capes = profile.as_ref().map(|p| p.capes.clone()).unwrap_or_default();
    if capes.is_empty() {
        return;
    }
    ui.add_space(14.0);
    w::section(ui, &t, "cape");
    let active = profile.as_ref().and_then(|p| p.cape()).map(|c| c.id.clone());
    let mut choices: Vec<(Option<String>, String)> = vec![(None, "none".into())];
    choices.extend(capes.iter().map(|c| (Some(c.id.clone()), c.alias.clone().unwrap_or_else(|| "cape".into()).to_lowercase())));
    let mut pick = None;
    for (id, label) in &choices {
        let on = *id == active;
        if w::menu_row(ui, &t, on, "", label, if on { "worn" } else { "" }).clicked() && !on {
            pick = Some(id.clone());
        }
    }
    if key(Nav::Cycle) {
        let at = choices.iter().position(|(id, _)| *id == active).unwrap_or(0);
        pick = Some(choices[(at + 1) % choices.len()].0.clone());
    }
    if let Some(id) = pick
        && !busy
    {
        skin_change(app, &ctx, &uuid, Change::Cape(id));
    }
}
