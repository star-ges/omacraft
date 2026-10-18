mod accounts;
mod config;
mod content;
mod export;
mod group_manager;
mod icon_picker;
mod instances;
mod logs;
mod mod_sets;
pub mod nav;
mod new_instance;
mod screenshots;
mod servers;
mod settings;
mod skin_view;
mod version;
mod version_picker;
pub mod widgets;
mod worlds;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, CornerRadius, Key, Layout, RichText, Sense, Stroke, Ui, vec2};

use crate::auth::{self, Account, Accounts, MsaAccount};
use crate::config::Config;
use crate::instance::{self, ContentKind, Instance};
use crate::launch::{self, GameLog, LaunchResult, LogLevel, QuickPlay, SharedLog};
use crate::shares::Kind;
use instances::{InstTab, ListRow};
use crate::keymap::{Action, Keymap};
use nav::{Nav, Zone};
use crate::mojang::{self, Manifest};
use crate::task::{Promise, Task, TaskStatus};
use crate::theme::{self, FontSet, Pending, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Instances,
    Accounts,
    Settings,
}

impl Tab {
    const ALL: [(Tab, &'static str, &'static str); 3] = [
        (Tab::Instances, "1", "instances"),
        (Tab::Accounts, "2", "accounts"),
        (Tab::Settings, "3", "settings"),
    ];
}

pub enum Modal {
    NewInstance(Box<new_instance::NewInstance>),
    Edit(Box<instances::EditState>),
    IconPicker(Box<icon_picker::IconPicker>),
    Groups(Box<group_manager::GroupManager>),
    Export(Box<export::ExportDialog>),
    Help,
    Confirm(Confirm),
    /// A world set member's version change.
    SetVersion(Box<version::SetVersion>),
    /// Pick a backup of an instance's mods to roll back to.
    RollBack(Box<content::RollBack>),
    /// After a rollback: roll a world played with the newer mods back too?
    WorldRollBack(Box<content::WorldPrompt>),
    /// An instance's mod sets.
    ModSets(Box<mod_sets::SetsDialog>),
    /// Starting a search for the breaking mod, or answering one of its steps.
    Bisect(Box<mod_sets::BisectDialog>),
    /// Leaving a config file with unsaved changes.
    ConfigLeave(Box<config::Leave>),
}

/// A yes/no question before something destructive.
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub button: String,
    pub action: ConfirmAction,
}

pub enum ConfirmAction {
    DeleteInstance(String),
    /// A world, screenshot or other file inside an instance.
    DeletePath(std::path::PathBuf),
    /// Back to the default skin, for this account UUID.
    ResetSkin(String),
    /// A share or world set no instance uses, with its files.
    DeleteSet(Kind, String),
    /// Put a world back from a backup: (instance id, the backup).
    RestoreWorld(String, crate::backups::Snapshot),
    DeleteBackup(crate::backups::Snapshot),
    /// Put an instance's mods and config back: (instance id, the backup).
    RollBack(String, crate::backups::Snapshot),
    /// A mod set: (instance id, set name). Only the set goes; the mods stay.
    DeleteModSet(String, String),
    /// Forget instance id's search for the breaking mod, leaving its mods as they are.
    ForgetSearch(String),
    /// Throw away the unsaved changes to the open config file.
    RevertConfig,
}

enum Phase {
    Auth(Promise<MsaAccount>, Instance, Option<QuickPlay>),
    Game(Promise<LaunchResult>),
    Ended,
}

pub struct Session {
    pub log: SharedLog,
    pub task: Task,
    phase: Phase,
}

impl Session {
    pub fn running(&self) -> bool {
        !matches!(self.phase, Phase::Ended)
    }

    /// Signing in or preparing, before Java starts: the mods are read for the play
    /// records just before it does, so they mustn't change meanwhile.
    pub fn starting(&self) -> bool {
        self.running() && self.log.lock().unwrap().pid.is_none()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum ToastKind {
    Info,
    Error,
}

pub struct Toast {
    text: String,
    kind: ToastKind,
    born: Instant,
}

impl Toast {
    pub fn info(text: impl Into<String>) -> Self {
        Toast { text: text.into(), kind: ToastKind::Info, born: Instant::now() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Toast { text: text.into(), kind: ToastKind::Error, born: Instant::now() }
    }
}

pub struct App {
    pub rt: tokio::runtime::Handle,
    pub t: Theme,
    watcher: Arc<Mutex<Pending>>,
    /// Fonts in use, re-resolved only when the watcher sees fontconfig change.
    pub fonts: FontSet,
    pub cfg: Config,
    pub accounts: Accounts,
    pub instances: Vec<Instance>,
    pub groups: crate::groups::Groups,
    pub sizes: SizeCache,
    pub selected: Option<String>,
    pub filter: String,
    pub focus_filter: bool,
    pub tab: Tab,
    pub inst_tab: InstTab,
    /// Which pane the keyboard drives.
    pub focus: Zone,
    /// This frame's navigation key, taken by whichever pane has focus.
    nav: Option<Nav>,
    /// Scroll the selected instance into view (after keyboard moves).
    pub scroll_list: bool,
    pub keymap: Keymap,
    /// Settings → keys waiting for a press: (action, chord to replace or None to add).
    pub capture: Option<(Action, Option<usize>)>,
    pub manifest: Option<Result<Manifest, String>>,
    manifest_p: Option<Promise<Manifest>>,
    pub sessions: HashMap<String, Session>,
    pub modal: Option<Modal>,
    pub content: content::ContentState,
    /// Mod sets and the search for a breaking mod, per instance.
    pub mod_sets: mod_sets::State,
    pub version: version::VersionState,
    pub worlds: worlds::WorldsState,
    pub servers: servers::ServersState,
    pub shots: screenshots::ShotsState,
    pub logs: logs::LogsState,
    pub config: config::ConfigState,
    pub acct: accounts::AccountsState,
    imports: Vec<(Task, Promise<crate::import::Imported>)>,
    /// (task, result, instance id)
    exports: Vec<(Task, Promise<crate::export::Exported>, String)>,
    /// Settings → reclaim disk space, while it runs.
    pub reclaim: Option<(Task, Promise<crate::store::Reclaimed>)>,
    /// Shared folders being linked or unlinked: (task, (notes, errors)).
    pub sharing: Option<(Task, Promise<(Vec<String>, Vec<String>)>)>,
    /// Share sets with a folder on disk, re-read when sharing changes.
    pub share_sets: Vec<String>,
    /// Share set → ids of instances whose folders link into it, re-read with `share_sets`.
    /// Can name an instance that has left on paper but whose leave hasn't finished.
    pub share_links: std::collections::BTreeMap<String, Vec<String>>,
    /// The same two for world sets.
    pub world_sets: Vec<String>,
    pub world_links: std::collections::BTreeMap<String, Vec<String>>,
    /// "new group" text in the instance context menu.
    pub group_buf: String,
    pub toasts: Vec<Toast>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle) -> Self {
        let t = Theme::load();
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        smithay_clipboard::set_drop_notify(move || ctx.request_repaint());
        let fonts = FontSet::resolve();
        fonts.apply(&cc.egui_ctx);
        t.apply(&cc.egui_ctx);
        let watcher = theme::spawn_watcher(cc.egui_ctx.clone());
        let instances = instance::load_all();
        let cfg = Config::load();
        let mut groups = crate::groups::Groups::load();
        if groups.sync(&instances) {
            let _ = groups.save();
        }
        let mut app = Self {
            groups,
            sizes: SizeCache::default(),
            rt,
            t,
            watcher,
            fonts,
            keymap: Keymap::new(&cfg.keys),
            cfg,
            accounts: Accounts::load(),
            selected: instances.first().map(|i| i.id.clone()),
            instances,
            filter: String::new(),
            focus_filter: false,
            tab: Tab::Instances,
            inst_tab: InstTab::Version,
            focus: Zone::List,
            nav: None,
            scroll_list: false,
            capture: None,
            manifest: None,
            manifest_p: None,
            sessions: HashMap::new(),
            modal: None,
            content: Default::default(),
            mod_sets: Default::default(),
            version: Default::default(),
            worlds: Default::default(),
            servers: Default::default(),
            shots: Default::default(),
            logs: Default::default(),
            config: Default::default(),
            acct: Default::default(),
            imports: Vec::new(),
            exports: Vec::new(),
            reclaim: None,
            sharing: None,
            share_sets: crate::shares::sets_on_disk(Kind::Packs),
            share_links: crate::shares::sets_in_use(Kind::Packs),
            world_sets: crate::shares::sets_on_disk(Kind::Worlds),
            world_links: crate::shares::sets_in_use(Kind::Worlds),
            group_buf: String::new(),
            toasts: Vec::new(),
        };
        app.load_manifest(&cc.egui_ctx);
        app
    }

    pub fn load_manifest(&mut self, ctx: &egui::Context) {
        self.manifest = None;
        self.manifest_p = Some(Promise::spawn(&self.rt, ctx, mojang::fetch_manifest()));
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), kind, born: Instant::now() });
    }

    pub fn selected_instance(&self) -> Option<&Instance> {
        let id = self.selected.as_deref()?;
        self.instances.iter().find(|i| i.id == id)
    }

    /// The instance list as drawn: ungrouped first, then each group under a
    /// header. Collapsed groups stay open while filtering.
    pub fn list_rows(&self) -> Vec<ListRow> {
        let f = self.filter.to_lowercase();
        let matches = |i: &Instance| {
            f.is_empty()
                || i.name.to_lowercase().contains(&f)
                || i.mc_version.contains(&f)
                || i.loader.display().contains(&f)
                || i.group.to_lowercase().contains(&f)
        };
        let mut rows = Vec::new();
        let ungrouped = self.instances.iter().enumerate().filter(|(_, i)| i.group.is_empty() && matches(i));
        rows.extend(ungrouped.map(|(n, _)| ListRow::Instance(n)));
        for g in self.groups.names() {
            let members: Vec<usize> = self.instances.iter().enumerate().filter(|(_, i)| i.group == g && matches(i)).map(|(n, _)| n).collect();
            if members.is_empty() {
                continue;
            }
            let collapsed = f.is_empty() && self.cfg.collapsed_groups.contains(&g);
            rows.push(ListRow::Group { name: g, count: members.len(), collapsed });
            if !collapsed {
                rows.extend(members.into_iter().map(ListRow::Instance));
            }
        }
        rows
    }

    /// Instance indices in list order, for keyboard movement.
    pub fn visible_instances(&self) -> Vec<usize> {
        self.list_rows()
            .into_iter()
            .filter_map(|r| match r {
                ListRow::Instance(n) => Some(n),
                ListRow::Group { .. } => None,
            })
            .collect()
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.sessions.get(id).is_some_and(|s| s.running())
    }

    // -- launching ----------------------------------------------------------

    pub fn launch_selected(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.selected.clone() {
            self.launch(ctx, &id, None);
        }
    }

    pub fn launch(&mut self, ctx: &egui::Context, id: &str, quick: Option<QuickPlay>) {
        let Some(inst) = self.instances.iter().find(|i| i.id == id).cloned() else { return };
        if self.is_running(&inst.id) {
            self.toast(ToastKind::Info, format!("{} is already running", inst.name));
            return;
        }
        // A restore or mod swap would change files under the game as it loads them.
        // So is a share or world set change: the launch's own `apply` would undo a leave halfway.
        let why = if self.sharing.is_some() { Some("shared folders are moving") } else { worlds::launch_blocked(&self.content, &self.worlds, &inst) };
        if let Some(why) = why {
            self.toast(ToastKind::Error, format!("Can't start {} while {why}", inst.name));
            return;
        }
        // The game would read the config as it is on disk, not as edited here.
        if let Some(o) = self.config.open.as_ref().filter(|o| o.inst_id == inst.id && o.dirty()) {
            let name = o.rel.display().to_string();
            self.toast(ToastKind::Error, format!("Save or revert your changes to {name} before playing"));
            return;
        }
        let Some(account) = self.accounts.active().cloned() else {
            self.toast(ToastKind::Error, "Add an account before playing");
            self.tab = Tab::Accounts;
            return;
        };
        // A new run: the last one's crash notice is old news.
        self.logs.notices.remove(id);
        if self.selected.as_deref() == Some(id) {
            self.inst_tab = InstTab::Log;
        }
        let log: SharedLog = Arc::new(Mutex::new(GameLog::default()));
        let task = Task::new(format!("launch {}", inst.name));
        let phase = match &account {
            Account::Msa(a) if a.mc_expires <= chrono::Utc::now() => {
                let client_id = self.cfg.client_id();
                task.stage("Refreshing Microsoft session", 0);
                log.lock().unwrap().push(LogLevel::Launcher, "refreshing Microsoft session…");
                let a = a.clone();
                Phase::Auth(
                    Promise::spawn(&self.rt, ctx, async move { auth::refresh(&client_id, &a).await }),
                    inst.clone(),
                    quick,
                )
            }
            _ => self.game_phase(ctx, &inst, account, task.clone(), log.clone(), quick),
        };
        self.sessions.insert(inst.id.clone(), Session { log, task, phase });
    }

    fn game_phase(&self, ctx: &egui::Context, inst: &Instance, account: Account, task: Task, log: SharedLog, quick: Option<QuickPlay>) -> Phase {
        let token = match &account {
            Account::Msa(a) => a.mc_token.clone(),
            Account::Offline { .. } => "0".into(),
        };
        let fut = launch::launch(task, inst.clone(), self.cfg.clone(), account, token, log, quick);
        Phase::Game(Promise::spawn(&self.rt, ctx, fut))
    }

    pub fn kill(&mut self, id: &str) {
        if let Some(pid) = self.sessions.get(id).and_then(|s| s.log.lock().unwrap().pid) {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if self.groups.sync(&self.instances) {
            let _ = self.groups.save();
        }
        if let Some(p) = &self.manifest_p
            && let Some(r) = p.take()
        {
            self.manifest = Some(r.map_err(|e| format!("{e:#}")));
            self.manifest_p = None;
        }

        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        let mut ended = false;
        for id in ids {
            let s = self.sessions.get_mut(&id).unwrap();
            match &s.phase {
                Phase::Auth(p, inst, quick) => {
                    let Some(r) = p.take() else { continue };
                    let (inst, quick) = (inst.clone(), quick.clone());
                    match r {
                        Ok(acct) => {
                            let account = Account::Msa(acct);
                            if self.accounts.refresh(account.clone()) {
                                let _ = self.accounts.save();
                            }
                            let (task, log) = {
                                let s = &self.sessions[&id];
                                (s.task.clone(), s.log.clone())
                            };
                            let phase = self.game_phase(ctx, &inst, account, task, log, quick);
                            self.sessions.get_mut(&id).unwrap().phase = phase;
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            s.log.lock().unwrap().push(LogLevel::Error, msg.clone());
                            s.task.finish(&Err(e));
                            s.phase = Phase::Ended;
                            self.toast(ToastKind::Error, msg);
                        }
                    }
                }
                Phase::Game(p) => {
                    let Some(r) = p.take() else { continue };
                    s.phase = Phase::Ended;
                    let lines: Vec<String> = s.log.lock().unwrap().lines.iter().map(|l| l.text.clone()).collect();
                    // The launch may have finished a share set change, and recorded worlds.
                    ended = true;
                    match r {
                        Ok(res) => {
                            if let Some(inst) = self.instances.iter_mut().find(|i| i.id == id) {
                                inst.last_played = Some(chrono::Utc::now());
                                inst.played_secs += res.played.as_secs();
                                let _ = inst.save();
                            }
                            instance::sort(&mut self.instances);
                            // A crash report since it started, or a failed exit: find out why.
                            if let Some(inst) = self.instances.iter().find(|i| i.id == id).cloned() {
                                let started = std::time::SystemTime::now() - res.played - Duration::from_secs(5);
                                self.logs.watch_exit(&self.rt, ctx, &inst, self.cfg.default_memory_mb, started, res.exit.clone(), lines);
                            }
                            // A step of a search for the breaking mod: ask how it went.
                            mod_sets::game_ended(self, &id, &res.exit);
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            s.log.lock().unwrap().push(LogLevel::Error, msg.clone());
                            self.toast(ToastKind::Error, msg);
                        }
                    }
                }
                Phase::Ended => {}
            }
        }
        logs::poll_crashes(self);
        if ended {
            self.refresh_shares();
            self.worlds.invalidate();
        }
        if self.sessions.values().any(|s| s.running()) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.imports).into_iter().partition(|(_, p)| p.ready());
        self.imports = running;
        for (task, p) in done {
            match p.take().unwrap() {
                Ok(crate::import::Imported { inst, warnings }) => {
                    self.toast(ToastKind::Info, format!("Imported {}", inst.name));
                    match warnings.as_slice() {
                        [] => {}
                        [w] => self.toast(ToastKind::Error, w.clone()),
                        [w, rest @ ..] => self.toast(ToastKind::Error, format!("{w} (and {} more)", rest.len())),
                    }
                    self.selected = Some(inst.id.clone());
                    self.instances.push(inst);
                    instance::sort(&mut self.instances);
                }
                Err(e) => {
                    task.finish(&Err(anyhow::anyhow!("{e:#}")));
                    self.toast(ToastKind::Error, format!("Import failed: {e:#}"));
                }
            }
        }

        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.exports).into_iter().partition(|(_, p, _)| p.ready());
        self.exports = running;
        for (task, p, _) in done {
            match p.take().unwrap() {
                Ok(out) => {
                    let name = out.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    self.toast(ToastKind::Info, format!("Exported {name}"));
                }
                Err(e) => {
                    task.finish(&Err(anyhow::anyhow!("{e:#}")));
                    self.toast(ToastKind::Error, format!("Export failed: {e:#}"));
                }
            }
        }

        if let Some((task, p)) = self.reclaim.take_if(|(_, p)| p.ready()) {
            let r = p.take().unwrap();
            task.finish(&r.as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e:#}")));
            match r {
                Ok(done) => self.toast(ToastKind::Info, done.summary()),
                Err(e) => self.toast(ToastKind::Error, format!("Couldn't reclaim space: {e:#}")),
            }
        }

        if let Some((task, p)) = self.sharing.take_if(|(_, p)| p.ready()) {
            let r = p.take().unwrap();
            task.finish(&r.as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e:#}")));
            match r {
                Ok((notes, errors)) => {
                    for e in &errors {
                        self.toast(ToastKind::Error, format!("Shared folders: {e}"));
                    }
                    match notes.as_slice() {
                        [] if errors.is_empty() => self.toast(ToastKind::Info, "Shared folders updated"),
                        [] => {}
                        [n] => self.toast(ToastKind::Info, n.clone()),
                        [n, rest @ ..] => self.toast(ToastKind::Info, format!("{n} (and {} more)", rest.len())),
                    }
                }
                Err(e) => self.toast(ToastKind::Error, format!("Shared folders: {e:#}")),
            }
            version::after_sharing(self);
            self.refresh_shares();
            self.shots.invalidate();
            self.content.invalidate();
            self.worlds.invalidate();
        }

        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.worlds.jobs).into_iter().partition(|j| j.promise.ready());
        self.worlds.jobs = running;
        for worlds::WorldJob { task, promise, inst_id: id, .. } in done {
            let r = promise.take().unwrap();
            task.finish(&r.as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e:#}")));
            match r {
                Ok(done) => {
                    if let Some(n) = done.note {
                        self.toast(ToastKind::Info, n);
                    }
                    self.worlds.finished(&id, done.select);
                }
                Err(e) => {
                    self.toast(ToastKind::Error, format!("{e:#}"));
                    self.worlds.invalidate();
                }
            }
        }
    }

    pub fn start_export_dialog(&mut self, ctx: &egui::Context, id: &str) {
        if self.sharing.is_some() {
            self.toast(ToastKind::Error, "Shared folders are moving; export again in a moment");
            return;
        }
        if let Some(inst) = self.instances.iter().find(|i| i.id == id).cloned() {
            self.modal = Some(Modal::Export(Box::new(export::ExportDialog::new(self, ctx, inst))));
        }
    }

    fn start_export(&mut self, ctx: &egui::Context, req: crate::export::Request) {
        // Checked again: sharing may have started while the dialog was open. An export
        // reading a folder mid-move would skip what had already left it.
        if self.sharing.is_some() {
            self.toast(ToastKind::Error, "Shared folders are moving; export again in a moment");
            return;
        }
        let task = Task::new("export");
        task.stage(format!("Exporting {}", req.inst.name), 0);
        let id = req.inst.id.clone();
        let p = Promise::spawn(&self.rt, ctx, crate::export::export(task.clone(), req));
        self.exports.push((task, p, id));
    }

    pub fn start_reclaim(&mut self, ctx: &egui::Context) {
        if self.reclaim.is_none() {
            let task = Task::new("reclaim");
            let p = Promise::spawn(&self.rt, ctx, crate::store::reclaim(task.clone()));
            self.reclaim = Some((task, p));
        }
    }

    pub fn refresh_shares(&mut self) {
        self.share_sets = crate::shares::sets_on_disk(Kind::Packs);
        self.share_links = crate::shares::sets_in_use(Kind::Packs);
        self.world_sets = crate::shares::sets_on_disk(Kind::Worlds);
        self.world_links = crate::shares::sets_in_use(Kind::Worlds);
    }

    /// Sets of this kind with a folder on disk.
    pub fn sets_on_disk(&self, kind: Kind) -> &[String] {
        match kind {
            Kind::Packs => &self.share_sets,
            Kind::Worlds => &self.world_sets,
        }
    }

    /// Set → ids of instances whose folders link into it now.
    pub fn set_links(&self, kind: Kind) -> &std::collections::BTreeMap<String, Vec<String>> {
        match kind {
            Kind::Packs => &self.share_links,
            Kind::Worlds => &self.world_links,
        }
    }

    /// Every set of this kind: on disk, named by an instance, or still linked into. Sorted.
    pub fn all_sets(&self, kind: Kind) -> Vec<String> {
        let mut sets = self.sets_on_disk(kind).to_vec();
        sets.extend(self.instances.iter().map(|i| kind.set_of(i).to_string()).filter(|s| !s.is_empty()));
        sets.extend(self.set_links(kind).keys().cloned());
        sets.sort_by_key(|s| s.to_lowercase());
        sets.dedup();
        sets
    }

    /// Bring these instances' shared folders in line with their settings. Running
    /// instances are left for their next launch, which does the same.
    pub fn start_sharing(&mut self, ctx: &egui::Context, insts: Vec<Instance>) {
        if self.sharing.is_some() {
            self.toast(ToastKind::Error, "Shared folders are still moving; try again in a moment");
            return;
        }
        let insts: Vec<Instance> = insts.into_iter().filter(|i| !self.is_running(&i.id)).collect();
        let task = Task::new("shared folders");
        task.stage("Updating shared folders", insts.len() as u64);
        let t = task.clone();
        let p = Promise::spawn(&self.rt, ctx, async move {
            Ok(tokio::task::spawn_blocking(move || {
                let (mut notes, mut errors) = (Vec::new(), Vec::new());
                for i in &insts {
                    match crate::shares::apply(i) {
                        Ok(n) => notes.extend(n),
                        Err(e) => errors.push(format!("{}: {e:#}", i.name)),
                    }
                    t.inc(1);
                }
                (notes, errors)
            })
            .await?)
        });
        self.sharing = Some((task, p));
    }

    pub fn start_import(&mut self, ctx: &egui::Context, req: crate::import::Request) {
        let task = Task::new("import");
        task.stage("Importing", 0);
        let p = Promise::spawn(&self.rt, ctx, crate::import::import(task.clone(), req));
        self.imports.push((task, p));
    }

    // -- chrome -------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut Ui) {
        let t = self.t.clone();
        let h = t.body_size() + 22.0;
        egui::Panel::top("top")
            .exact_size(h)
            .frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("󰍳 Omacraft").font(theme::bold(t.body_size())).color(t.accent));
                    ui.add_space(18.0);
                    for (tab, key, label) in Tab::ALL {
                        let on = self.tab == tab;
                        let color = if on { t.fg_light } else { t.muted };
                        let text = RichText::new(format!("{key} {label}")).color(color);
                        let r = ui.add(egui::Label::new(text).sense(Sense::click()));
                        if on {
                            let y = r.rect.bottom() + (h - r.rect.height()) / 2.0 - 3.0;
                            ui.painter().hline(r.rect.x_range(), y, Stroke::new(2.0, t.accent));
                        }
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.tab = tab;
                        }
                        ui.add_space(10.0);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        match self.accounts.active() {
                            Some(a) => {
                                ui.label(RichText::new(a.kind()).color(t.muted));
                                ui.label(RichText::new(format!("󰀄 {}", a.name())).color(t.fg));
                            }
                            None => {
                                if ui
                                    .add(egui::Label::new(RichText::new("󰀄 no account").color(t.yellow)).sense(Sense::click()))
                                    .clicked()
                                {
                                    self.tab = Tab::Accounts;
                                }
                            }
                        }
                        let running: Vec<&str> = self
                            .sessions
                            .iter()
                            .filter(|(_, s)| s.running())
                            .filter_map(|(id, _)| self.instances.iter().find(|i| &i.id == id).map(|i| i.name.as_str()))
                            .collect();
                        if !running.is_empty() {
                            ui.add_space(14.0);
                            ui.label(RichText::new(format!("● {}", running.join(", "))).color(t.green));
                        }
                    });
                });
                let r = ui.max_rect();
                ui.painter().hline(r.x_range(), r.bottom(), Stroke::new(1.0, t.fg_a(0.12)));
            });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let t = self.t.clone();
        egui::Panel::bottom("status")
            .exact_size(t.body_size() + 18.0)
            .frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, t.fg_a(0.12)));
                ui.horizontal_centered(|ui| {
                    let hints = nav::hints(self);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Show the most recent unfinished task.
                        let task = self
                            .sessions
                            .values()
                            .map(|s| &s.task)
                            .chain(self.content.tasks())
                            .chain(self.imports.iter().map(|(t, _)| t))
                            .chain(self.exports.iter().map(|(t, ..)| t))
                            .chain(self.worlds.tasks())
                            .chain(self.reclaim.iter().map(|(t, _)| t))
                            .find(|t| t.status() == TaskStatus::Running)
                            .cloned();
                        if let Some(task) = task {
                            widgets::progress(ui, &t, task.fraction(), 140.0);
                            let stage = task.stage_text();
                            let text = if stage.is_empty() { task.0.label.clone() } else { stage };
                            ui.label(RichText::new(text).color(t.muted));
                            ui.ctx().request_repaint_after(Duration::from_millis(100));
                        }
                        // Hints get whatever is left, clipped rather than overlapping.
                        ui.add_space(16.0);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.shrink_clip_rect(ui.max_rect());
                            for (k, l) in &hints {
                                widgets::key_hint(ui, &t, k, l);
                            }
                        });
                    });
                });
            });
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let t = self.t.clone();
        self.toasts.retain(|n| n.born.elapsed() < Duration::from_secs(if n.kind == ToastKind::Error { 9 } else { 5 }));
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        let mut dismiss = None;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_TOP, vec2(-14.0, t.body_size() + 34.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (i, n) in self.toasts.iter().enumerate().rev() {
                    let border = if n.kind == ToastKind::Error { t.red } else { t.border_active };
                    let r = egui::Frame::new()
                        .fill(t.bg)
                        .stroke(Stroke::new(2.0, border))
                        .inner_margin(12)
                        .corner_radius(CornerRadius::ZERO)
                        .show(ui, |ui| {
                            ui.set_width(340.0);
                            widgets::para(ui, RichText::new(&n.text).color(t.fg));
                        });
                    if r.response.interact(Sense::click()).clicked() {
                        dismiss = Some(i);
                    }
                    ui.add_space(8.0);
                }
            });
        if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
    }

    fn keys(&mut self, ctx: &egui::Context) {
        use Action::*;
        if let Some(target) = self.capture {
            self.capture_key(ctx, target);
            return;
        }
        if self.config.capture.is_some() {
            config::capture(self, ctx);
            ctx.request_repaint();
            return;
        }
        let km = self.keymap.clone();
        if km.take(ctx, &[Quit]).is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Saving a config works while typing in it.
        if self.tab == Tab::Instances && self.inst_tab == InstTab::Config && self.modal.is_none() && km.take(ctx, &[SaveConfig]).is_some() {
            self.config.key = Some(SaveConfig);
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if self.modal.is_some() {
            let mut cands = nav::PANE_ACTIONS.to_vec();
            cands.extend([FocusLeft, FocusRight]);
            self.nav = km.take(ctx, &cands).and_then(|a| nav::to_nav(a, true));
            return;
        }
        let mut cands = vec![Help, TabInstances, TabAccounts, TabSettings, Play, New, Import, Export, Groups];
        match self.tab {
            Tab::Instances => {
                cands.extend([FocusLeft, FocusRight, PrevTab, NextTab, Back, ModsTab, ChangeIcon, LoaderType, Update, UpdateOne, RollBack, Why, ModSets, FindBreaking, SaveConfig, RawConfig, ConfigArea]);
                cands.extend(nav::PANE_ACTIONS);
            }
            Tab::Accounts => cands.extend([Microsoft, Down, Up, Top, Bottom, Activate, Delete, Add, TurnLeft, TurnRight, UploadSkin, Toggle, Cycle, Back]),
            Tab::Settings => {}
        }
        let Some(a) = km.take(ctx, &cands) else { return };
        match a {
            Help => self.modal = Some(Modal::Help),
            TabInstances => self.tab = Tab::Instances,
            TabAccounts => self.tab = Tab::Accounts,
            TabSettings => self.tab = Tab::Settings,
            Play => self.launch_selected(ctx),
            New => self.modal = Some(Modal::NewInstance(Box::new(new_instance::NewInstance::new(&self.cfg)))),
            Import => self.modal = Some(Modal::NewInstance(Box::new(new_instance::NewInstance::import(&self.cfg, String::new())))),
            Export => {
                if let Some(id) = self.selected.clone() {
                    self.start_export_dialog(ctx, &id);
                }
            }
            Groups => {
                let group = self.selected_instance().map(|i| i.group.clone()).filter(|g| !g.is_empty());
                self.modal = Some(Modal::Groups(Box::new(group_manager::GroupManager::new(group))));
            }
            Microsoft => self.acct.start_microsoft = true,
            TurnLeft => self.acct.turn_by(-1.0),
            TurnRight => self.acct.turn_by(1.0),
            UploadSkin => self.acct.start_upload = true,
            Back if self.tab == Tab::Accounts => self.acct.cancel_pending(),
            _ if self.tab == Tab::Accounts => self.nav = nav::to_nav(a, false),
            _ => self.instance_key(ctx, a),
        }
        if self.focus == Zone::Side && !nav::has_side(self) {
            self.focus = Zone::Main;
        }
    }

    fn instance_key(&mut self, ctx: &egui::Context, a: Action) {
        match a {
            Action::PrevTab => self.inst_tab = self.inst_tab.step(-1),
            Action::NextTab => self.inst_tab = self.inst_tab.step(1),
            Action::ModsTab => self.inst_tab = InstTab::Mods,
            Action::ChangeIcon => {
                if let Some(id) = self.selected.clone() {
                    self.modal = Some(Modal::IconPicker(Box::new(icon_picker::IconPicker::new(id))));
                }
            }
            Action::LoaderType => {
                if self.inst_tab == InstTab::Version {
                    self.version.cycle_loader();
                }
            }
            Action::Update | Action::UpdateOne | Action::RollBack | Action::ModSets => {
                if self.inst_tab == InstTab::Mods {
                    self.content.key = Some(a);
                }
            }
            Action::Why => {
                if matches!(self.inst_tab, InstTab::Log | InstTab::LogHistory) {
                    self.logs.key = Some(a);
                }
            }
            Action::FindBreaking => {
                if let Some(id) = self.selected.clone() {
                    mod_sets::key(self, &id);
                }
            }
            Action::SaveConfig | Action::RawConfig | Action::ConfigArea => {
                if self.inst_tab == InstTab::Config {
                    self.config.key = Some(a);
                }
            }
            Action::FocusLeft => {
                self.focus = match self.focus {
                    Zone::Side => Zone::Main,
                    _ => Zone::List,
                }
            }
            Action::FocusRight => {
                self.focus = match self.focus {
                    Zone::List => Zone::Main,
                    _ if nav::has_side(self) => Zone::Side,
                    z => z,
                }
            }
            Action::Back => {
                if self.inst_tab == InstTab::Version && self.version.picker_open() {
                    self.version.close();
                    self.focus = Zone::Main;
                } else if self.focus != Zone::List {
                    self.focus = Zone::List;
                } else {
                    self.filter.clear();
                }
            }
            _ => {
                let Some(n) = nav::to_nav(a, false) else { return };
                if self.focus == Zone::List {
                    self.list_nav(ctx, n);
                } else {
                    self.nav = Some(n);
                }
            }
        }
    }

    fn list_nav(&mut self, ctx: &egui::Context, n: Nav) {
        let vis = self.visible_instances();
        let pos = self.selected.as_ref().and_then(|id| vis.iter().position(|&i| &self.instances[i].id == id));
        if n.is_move() {
            if let Some(p) = n.step(vis.len(), pos) {
                self.selected = Some(self.instances[vis[p]].id.clone());
                self.scroll_list = true;
            }
            return;
        }
        let Some(inst) = self.selected_instance().cloned() else { return };
        match n {
            Nav::Activate => self.launch(ctx, &inst.id, None),
            Nav::Edit | Nav::Rename => self.modal = Some(Modal::Edit(Box::new(instances::EditState::from(&inst)))),
            Nav::Folder => open_path(&inst.game_dir()),
            Nav::Delete => self.confirm_delete_instance(&inst.id),
            Nav::Search => self.focus_filter = true,
            _ => {}
        }
    }

    /// Settings → keys is waiting for a key: take the next press.
    fn capture_key(&mut self, ctx: &egui::Context, (action, index): (Action, Option<usize>)) {
        let pressed = ctx.input_mut(|i| {
            let n = i.events.iter().position(|e| matches!(e, egui::Event::Key { pressed: true, .. }))?;
            match i.events.remove(n) {
                egui::Event::Key { key, modifiers, .. } => Some((key, modifiers)),
                _ => None,
            }
        });
        let Some((key, mods)) = pressed else { return };
        self.capture = None;
        if key == Key::Escape && !mods.any() {
            return;
        }
        let chord = crate::keymap::Chord::from_event(key, mods);
        let mut chords = self.keymap.chords(action).to_vec();
        chords.retain(|c| *c != chord);
        match index {
            Some(i) if i < chords.len() => chords[i] = chord,
            Some(i) => chords.insert(i.min(chords.len()), chord),
            None => chords.push(chord),
        }
        self.set_keys(action, chords);
    }

    /// Save an action's chords, dropping the override if it matches the default.
    pub fn set_keys(&mut self, action: Action, chords: Vec<crate::keymap::Chord>) {
        let info = crate::keymap::info(action);
        let specs: Vec<String> = chords.iter().map(|c| c.spec()).collect();
        let defaults: Vec<String> = info.defaults.iter().filter_map(|d| crate::keymap::Chord::parse(d)).map(|c| c.spec()).collect();
        if specs == defaults {
            self.cfg.keys.remove(info.id);
        } else {
            self.cfg.keys.insert(info.id.to_string(), specs);
        }
        self.keymap = Keymap::new(&self.cfg.keys);
        if let Err(e) = self.cfg.save() {
            self.toast(ToastKind::Error, format!("Couldn't save keys: {e}"));
        }
    }

    /// The pending navigation key, if `zone` has focus.
    pub fn nav_for(&mut self, zone: Zone) -> Option<Nav> {
        // An open dialog's keys are its own, though the panes under it still draw first.
        if self.modal.is_some() || (self.tab == Tab::Instances && self.focus != zone) {
            return None;
        }
        self.nav.take()
    }

    /// Navigation inside an open dialog.
    pub fn modal_nav(&mut self) -> Option<Nav> {
        self.nav.take()
    }

    fn refresh_theme(&mut self, ctx: &egui::Context) {
        let pending = std::mem::take(&mut *self.watcher.lock().unwrap());
        if let Some(fonts) = pending.fonts {
            fonts.apply(ctx);
            self.fonts = fonts;
        }
        if let Some(theme) = pending.theme
            && theme != self.t
        {
            theme.apply(ctx);
            self.t = theme;
        }
    }
}

impl App {
    fn run_confirmed(&mut self, ctx: &egui::Context, action: &ConfirmAction) {
        match action {
            ConfirmAction::DeleteInstance(id) => {
                let Some(inst) = self.instances.iter().find(|i| &i.id == id) else { return };
                let name = inst.name.clone();
                // Checked again here: something may have started since the dialog opened.
                if let Some(why) = self.instance_busy(id) {
                    self.toast(ToastKind::Error, format!("Can't delete {name} while {why}"));
                    return;
                }
                // Its backups are locked first: a job in another Omacraft may be using them.
                let backups = match crate::backups::lock_instance(id) {
                    Ok(b) => b,
                    Err(e) => {
                        self.toast(ToastKind::Error, format!("Can't delete {name} while a backup job is using {e:#}"));
                        return;
                    }
                };
                if let Err(e) = std::fs::remove_dir_all(inst.dir()) {
                    self.toast(ToastKind::Error, format!("Delete failed: {e}"));
                    return;
                }
                match backups.forget() {
                    Ok(()) => self.toast(ToastKind::Info, format!("Deleted {name}")),
                    Err(e) => self.toast(ToastKind::Error, format!("Deleted {name}, but not all its backups: {e:#}")),
                }
                self.instances.retain(|i| &i.id != id);
                self.sessions.remove(id);
                self.selected = self.visible_instances().first().map(|&n| self.instances[n].id.clone());
            }
            ConfirmAction::DeletePath(p) => {
                let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
                if let Err(e) = r {
                    self.toast(ToastKind::Error, format!("Delete failed: {e}"));
                }
                self.worlds.invalidate();
                self.shots.invalidate();
                self.content.invalidate();
            }
            ConfirmAction::RestoreWorld(id, snap) => worlds::start_restore(self, ctx, id, snap.clone()),
            ConfirmAction::DeleteBackup(snap) => {
                // A world job or an update's backups may be writing next to it.
                if !self.worlds.jobs.is_empty() || self.content.backing_up_worlds() {
                    self.toast(ToastKind::Error, "Can't delete a backup while worlds are being backed up or restored");
                } else if let Err(e) = crate::backups::delete(snap) {
                    self.toast(ToastKind::Error, format!("Couldn't delete the backup: {e:#}"));
                }
                self.worlds.invalidate();
            }
            ConfirmAction::RollBack(id, snap) => content::start_roll_back(self, ctx, id, snap.clone()),
            ConfirmAction::DeleteModSet(id, name) => mod_sets::delete_set(self, id, name),
            ConfirmAction::ForgetSearch(id) => mod_sets::forget_search(self, ctx, id),
            ConfirmAction::RevertConfig => config::revert(self),
            ConfirmAction::ResetSkin(uuid) => accounts::skin_change(self, ctx, uuid, crate::skins::Change::Reset),
            ConfirmAction::DeleteSet(kind, name) => {
                // Checked again: an instance may have joined since the dialog opened.
                // `delete_set` also refuses while any instance still links into the set.
                self.refresh_shares();
                if self.sharing.is_some() || self.instances.iter().any(|i| kind.set_of(i) == name) || self.set_links(*kind).contains_key(name) {
                    self.toast(ToastKind::Error, format!("{name} is in use"));
                } else if let Err(e) = crate::shares::delete_set(*kind, name) {
                    self.toast(ToastKind::Error, format!("{e:#}"));
                }
                self.refresh_shares();
            }
        }
    }

    /// While files are dragged over the window, say what dropping them does.
    fn drop_hint(&self, ctx: &egui::Context) {
        if !smithay_clipboard::hovering() && ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let t = &self.t;
        let text = match self.drop_target() {
            Some((_, DropTarget::Content(kind))) => format!("drop to add {}s", kind.noun()),
            Some((_, DropTarget::Worlds)) => "drop to add worlds (folders or .zip)".into(),
            None if self.tab == Tab::Accounts && self.modal.is_none() => "drop a skin (.png) or a pack to import".into(),
            None => "drop to import".into(),
        };
        let rect = ctx.content_rect();
        let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("drop-hint")));
        p.rect_filled(rect, 0.0, widgets::scrim(t));
        p.rect_stroke(rect.shrink(6.0), 0.0, Stroke::new(2.0, t.border_active), egui::StrokeKind::Inside);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, format!("\u{f019}  {text}"), egui::FontId::monospace(t.body_size() + 4.0), t.fg);
    }

    /// The tab a drop would add files to: mods, packs or worlds, with no dialog up.
    fn drop_target(&self) -> Option<(Instance, DropTarget)> {
        if self.tab != Tab::Instances || self.modal.is_some() {
            return None;
        }
        let target = match self.inst_tab {
            InstTab::Mods => DropTarget::Content(ContentKind::Mods),
            InstTab::ResourcePacks => DropTarget::Content(ContentKind::ResourcePacks),
            InstTab::ShaderPacks => DropTarget::Content(ContentKind::ShaderPacks),
            InstTab::Worlds => DropTarget::Worlds,
            _ => return None,
        };
        Some((self.selected_instance()?.clone(), target))
    }

    /// Files dropped on a mods, packs or worlds tab go into that folder; anywhere else (or
    /// a `.mrpack`) they open the import dialog.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        // winit only reports drops on X11; on Wayland they come through the patched clipboard crate.
        let mut paths: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        paths.extend(smithay_clipboard::take_dropped());
        if paths.is_empty() {
            return;
        }
        let packs = paths.iter().all(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mrpack")));
        match self.drop_target().filter(|_| !packs) {
            Some((inst, DropTarget::Content(kind))) => return content::add_dropped(self, ctx, &inst, kind, paths),
            Some((inst, DropTarget::Worlds)) => return worlds::add_dropped(self, ctx, &inst, paths),
            None => {}
        }
        let path = paths.swap_remove(0);
        let png = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"));
        if self.tab == Tab::Accounts && png && self.modal.is_none() && accounts::drop_file(self, &path) {
            return;
        }
        let source = path.display().to_string();
        match &mut self.modal {
            Some(Modal::NewInstance(s)) => {
                s.mode = new_instance::Mode::Import;
                s.source = source;
            }
            _ => self.modal = Some(Modal::NewInstance(Box::new(new_instance::NewInstance::import(&self.cfg, source)))),
        }
        self.tab = Tab::Instances;
    }
}

/// Where files dropped on the window go, besides the import dialog.
enum DropTarget {
    Content(ContentKind),
    Worlds,
}

/// Instance folder sizes, measured in the background and refreshed now and then.
#[derive(Default)]
pub struct SizeCache {
    sizes: Arc<Mutex<HashMap<String, (u64, Instant)>>>,
    pending: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl SizeCache {
    pub fn get(&self, inst: &Instance, rt: &tokio::runtime::Handle, ctx: &egui::Context) -> Option<u64> {
        let cached = self.sizes.lock().unwrap().get(&inst.id).copied();
        let stale = cached.is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(60));
        if stale && self.pending.lock().unwrap().insert(inst.id.clone()) {
            let (id, dir, sizes, pending, ctx) = (inst.id.clone(), inst.dir(), self.sizes.clone(), self.pending.clone(), ctx.clone());
            rt.spawn_blocking(move || {
                let n = crate::fsutil::tree_size(&dir);
                sizes.lock().unwrap().insert(id.clone(), (n, Instant::now()));
                pending.lock().unwrap().remove(&id);
                ctx.request_repaint();
            });
        }
        cached.map(|(n, _)| n)
    }
}

pub fn open_path(p: &std::path::Path) {
    let _ = std::fs::create_dir_all(p);
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

pub fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh_theme(&ctx);
        self.poll(&ctx);
        self.content.poll(&mut self.toasts);
        self.mod_sets.tick(&self.rt, &ctx);
        self.acct.poll(&mut self.accounts, &mut self.toasts);
        self.keys(&ctx);
        self.dropped_files(&ctx);

        self.top_bar(ui);
        self.status_bar(ui);
        let t = self.t.clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t.bg))
            .show(ui, |ui| match self.tab {
                Tab::Instances => instances::show(self, ui),
                Tab::Accounts => accounts::show(self, ui),
                Tab::Settings => settings::show(self, ui),
            });

        if self.modal.is_none()
            && let Some(p) = self.content.world_prompts.pop_front()
        {
            self.modal = Some(Modal::WorldRollBack(Box::new(p)));
        }
        config::guard(self, &ctx);
        self.show_modal(&ctx);
        self.toasts(&ctx);
        self.drop_hint(&ctx);
        self.nav = None;
    }
}

impl App {
    fn show_modal(&mut self, ctx: &egui::Context) {
        let Some(modal) = self.modal.take() else { return };
        let t = self.t.clone();
        let keep = match modal {
            Modal::NewInstance(mut s) => {
                let r = egui::Modal::new(egui::Id::new("new-instance"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| new_instance::show(self, &mut s, ui));
                match r.inner {
                    Some(new_instance::Outcome::Created(inst)) => {
                        self.selected = Some(inst.id.clone());
                        self.toast(ToastKind::Info, format!("Created {}", inst.name));
                        self.instances.push(inst);
                        instance::sort(&mut self.instances);
                        None
                    }
                    Some(new_instance::Outcome::Import(req)) => {
                        self.start_import(ctx, req);
                        None
                    }
                    None if r.should_close() || s.cancelled => None,
                    None => Some(Modal::NewInstance(s)),
                }
            }
            Modal::Edit(mut s) => {
                let r = egui::Modal::new(egui::Id::new("edit-instance"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| instances::edit(self, &mut s, ui));
                match r.inner {
                    instances::EditOutcome::Keep if !r.should_close() => Some(Modal::Edit(s)),
                    instances::EditOutcome::Delete => {
                        self.confirm_delete_instance(&s.id);
                        self.modal.take()
                    }
                    _ => None,
                }
            }
            Modal::IconPicker(mut s) => {
                let r = egui::Modal::new(egui::Id::new("icon-picker"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| icon_picker::show(self, &mut s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::IconPicker(s)) }
            }
            Modal::Export(mut s) => {
                let r = egui::Modal::new(egui::Id::new("export"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| export::show(self, &mut s, ui));
                match r.inner {
                    Some(Ok(req)) => {
                        self.start_export(ctx, req);
                        None
                    }
                    Some(Err(())) => None,
                    None if r.should_close() => None,
                    None => Some(Modal::Export(s)),
                }
            }
            Modal::Help => {
                let r = egui::Modal::new(egui::Id::new("help"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| nav::help(self, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::Help) }
            }
            Modal::Groups(mut s) => {
                let r = egui::Modal::new(egui::Id::new("groups"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| group_manager::show(self, &mut s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::Groups(s)) }
            }
            Modal::SetVersion(s) => {
                let r = egui::Modal::new(egui::Id::new("set-version"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| version::set_version_dialog(self, &s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::SetVersion(s)) }
            }
            Modal::RollBack(mut s) => {
                let r = egui::Modal::new(egui::Id::new("roll-back"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| content::roll_back_dialog(self, &mut s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::RollBack(s)) }
            }
            Modal::WorldRollBack(mut s) => {
                let r = egui::Modal::new(egui::Id::new("world-roll-back"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| content::world_prompt_dialog(self, &mut s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::WorldRollBack(s)) }
            }
            Modal::ModSets(mut s) => {
                let r = egui::Modal::new(egui::Id::new("mod-sets"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| mod_sets::sets_dialog(self, &mut s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::ModSets(s)) }
            }
            Modal::Bisect(mut s) => {
                let r = egui::Modal::new(egui::Id::new("bisect"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| mod_sets::bisect_dialog(self, &mut s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::Bisect(s)) }
            }
            Modal::ConfigLeave(s) => {
                let r = egui::Modal::new(egui::Id::new("config-leave"))
                    .frame(widgets::modal_frame(&t))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| config::leave_dialog(self, &s, ui));
                if r.inner || r.should_close() { None } else { Some(Modal::ConfigLeave(s)) }
            }
            Modal::Confirm(c) => {
                let mut yes = false;
                // ⏎ arrives as Activate; a raw Enter only if a widget had focus.
                let activate = self.modal_nav() == Some(Nav::Activate);
                let r = egui::Modal::new(egui::Id::new("confirm"))
                    .frame(widgets::modal_frame(&t).stroke(Stroke::new(2.0, t.red)))
                    .backdrop_color(widgets::scrim(&t))
                    .show(ctx, |ui| {
                        ui.set_width(420.0);
                        widgets::heading(ui, &t, &c.title);
                        ui.add_space(8.0);
                        widgets::para(ui, &c.body);
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            if widgets::danger_button(ui, &t, &c.button).clicked() || activate || ui.input(|i| i.key_pressed(Key::Enter)) {
                                yes = true;
                                return true;
                            }
                            widgets::button(ui, "cancel").clicked()
                        })
                        .inner
                    });
                if yes {
                    self.run_confirmed(ctx, &c.action);
                }
                if r.inner || r.should_close() { None } else { Some(Modal::Confirm(c)) }
            }
        };
        if self.modal.is_none() {
            self.modal = keep;
        }
    }
}
