//! Mods, resource packs and shader packs: what's installed on the left,
//! Modrinth on the right.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, CornerRadius, Layout, RichText, Stroke, Ui, vec2};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, Confirm, ConfirmAction, Modal, Toast, ToastKind};
use crate::instance::{ContentKind, Instance, ModFile};
use crate::content_meta::{self, FileMeta};
use crate::keymap::Action;
use crate::modrinth::{self, Hit, SearchResponse, Update};
use crate::task::{Promise, Task};

#[derive(Default)]
struct KindState {
    query: String,
    /// (instance id, query) of the last search sent.
    searched: Option<(String, String)>,
    /// When the query last changed, so a search goes out once typing pauses.
    typed: Option<Instant>,
    search_p: Option<Promise<SearchResponse>>,
    results: Option<Result<Vec<Hit>, String>>,
    /// (instance id, project id) → the install running for it.
    installing: HashMap<(String, String), Job>,
    selected: Option<String>,
    /// Keyboard selection in the Modrinth results.
    result: Option<usize>,
    cache: Option<(String, Vec<ModFile>, Instant)>,
}

struct Job {
    task: Task,
    promise: Promise<modrinth::Installed>,
    title: String,
    /// Where the files go, so they can be tagged with their project when done.
    dir: PathBuf,
}

#[derive(Default)]
pub struct ContentState {
    kinds: HashMap<ContentKind, KindState>,
    meta: MetaStore,
    remote: RemoteIcons,
    pub focus_search: bool,
    /// An update or rollback key for the mods tab, taken while it draws.
    pub key: Option<Action>,
    /// The last mod update check: (instance id, the check).
    updates: Option<(String, Check)>,
    /// Mods being updated or rolled back: (task, what to say when done, instance id).
    pub mod_job: Option<(Task, Promise<ModJobDone>, String)>,
    /// Worlds a rollback left played with newer mods, each to ask about in turn.
    pub world_prompts: std::collections::VecDeque<WorldPrompt>,
}

/// How a mods update or rollback went.
pub struct ModJobDone {
    kind: ToastKind,
    text: String,
    worlds: Vec<WorldPrompt>,
}

impl ModJobDone {
    fn new(kind: ToastKind, text: String) -> Self {
        ModJobDone { kind, text, worlds: Vec::new() }
    }
}

enum Check {
    Running(Promise<Vec<Update>>),
    Found(Vec<Update>),
}

/// Name, version and icon of installed files, filled in the background.
#[derive(Default, Clone)]
struct MetaStore {
    known: Arc<Mutex<HashMap<String, FileMeta>>>,
    pending: Arc<Mutex<HashSet<String>>>,
    /// Project of each file we installed ourselves (by enabled path), so it
    /// counts as installed before the hash lookup comes back.
    ours: Arc<Mutex<HashMap<PathBuf, String>>>,
}

/// The file's path as if enabled.
fn base_path(f: &ModFile) -> PathBuf {
    f.path.with_file_name(f.name.trim_end_matches(".disabled"))
}

/// Same key whether or not the file is disabled; a replaced file gets a new one.
fn meta_key(f: &ModFile) -> String {
    let mtime = f.modified.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    format!("{}|{}|{mtime}", base_path(f).display(), f.size)
}

impl MetaStore {
    fn get(&self, f: &ModFile) -> Option<FileMeta> {
        self.known.lock().unwrap().get(&meta_key(f)).cloned()
    }

    /// Modrinth projects among these files (one instance's, as they are now).
    fn project_ids(&self, files: &[ModFile]) -> HashSet<String> {
        let (known, ours) = (self.known.lock().unwrap(), self.ours.lock().unwrap());
        files
            .iter()
            .filter_map(|f| known.get(&meta_key(f)).and_then(|m| m.project_id.clone()).or_else(|| ours.get(&base_path(f)).cloned()))
            .collect()
    }

    fn installed(&self, dir: &std::path::Path, done: &modrinth::Installed) {
        let mut ours = self.ours.lock().unwrap();
        for (file, project) in done.files.iter().zip(&done.projects) {
            ours.insert(dir.join(file), project.clone());
        }
    }

    fn ensure(&self, files: &[ModFile], kind: ContentKind, rt: &tokio::runtime::Handle, ctx: &egui::Context) {
        let missing: Vec<(String, ModFile)> = {
            let known = self.known.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            files
                .iter()
                .map(|f| (meta_key(f), f.clone()))
                .filter(|(k, _)| !known.contains_key(k) && pending.insert(k.clone()))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let (store, ctx) = (self.clone(), ctx.clone());
        rt.spawn(async move {
            let keys: Vec<String> = missing.iter().map(|(k, _)| k.clone()).collect();
            let locals = tokio::task::spawn_blocking(move || {
                missing.into_iter().map(|(k, f)| (k, content_meta::read_local(&f.path, f.is_dir, kind))).collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            store.known.lock().unwrap().extend(locals.iter().cloned());
            ctx.request_repaint();

            // Modrinth fills gaps (and is the only source for shader icons).
            let hashes: Vec<String> = locals.iter().filter_map(|(_, m)| m.sha1.clone()).collect();
            if let Ok(found) = content_meta::lookup(hashes).await {
                for (k, m) in locals {
                    let Some(p) = m.sha1.as_ref().and_then(|h| found.get(h)) else { continue };
                    let icon = match (&m.icon, &p.icon_url) {
                        (None, Some(url)) => content_meta::fetch_icon(url).await.ok(),
                        _ => None,
                    };
                    if let Some(e) = store.known.lock().unwrap().get_mut(&k) {
                        e.project_id = Some(p.id.clone());
                        e.name = e.name.take().or(Some(p.title.clone()));
                        e.icon = e.icon.take().or(icon);
                    }
                    ctx.request_repaint();
                }
            }
            let mut pending = store.pending.lock().unwrap();
            for k in keys {
                pending.remove(&k);
            }
        });
    }
}

/// Icons for search results, downloaded once into the cache.
#[derive(Default, Clone)]
struct RemoteIcons {
    map: Arc<Mutex<HashMap<String, Option<PathBuf>>>>,
}

impl RemoteIcons {
    fn get(&self, url: &str, rt: &tokio::runtime::Handle, ctx: &egui::Context) -> Option<PathBuf> {
        let mut map = self.map.lock().unwrap();
        if let Some(p) = map.get(url) {
            return p.clone();
        }
        map.insert(url.to_string(), None);
        let (map, url, ctx) = (self.map.clone(), url.to_string(), ctx.clone());
        rt.spawn(async move {
            if let Ok(p) = content_meta::fetch_icon(&url).await {
                map.lock().unwrap().insert(url, Some(p));
                ctx.request_repaint();
            }
        });
        None
    }
}

/// Whether a file's name (no extension) is this project's: the slug alone, or the
/// slug then a version, maybe after loader tags (`sodium-0.6.0`, `sodium-fabric-0.5.8+mc1.20.1`),
/// so `sodium-extra-0.5` isn't Sodium.
fn is_slug_file(name: &str, slug: &str) -> bool {
    let (name, slug) = (name.to_lowercase(), slug.to_lowercase());
    let Some(rest) = name.strip_prefix(&slug) else { return false };
    if rest.is_empty() {
        return true;
    }
    let sep = |c: char| matches!(c, '-' | '_' | '+' | ' ');
    if !rest.starts_with(sep) {
        return false;
    }
    let version = |t: &str| {
        let t = t.strip_prefix("mc").or_else(|| t.strip_prefix('v')).unwrap_or(t);
        t.starts_with(|c: char| c.is_ascii_digit())
    };
    for token in rest.split(sep).filter(|t| !t.is_empty()) {
        if version(token) {
            return true;
        }
        if !matches!(token, "fabric" | "quilt" | "forge" | "neoforge") {
            return false;
        }
    }
    false
}

fn file_image(p: &std::path::Path) -> egui::ImageSource<'static> {
    egui::ImageSource::Uri(format!("file://{}", p.display()).into())
}

fn kind_glyph(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Mods => "󰏗",
        ContentKind::ResourcePacks => "󰋩",
        ContentKind::ShaderPacks => "󰖨",
    }
}

impl ContentState {
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.kinds.values().flat_map(|k| k.installing.values().map(|j| &j.task)).chain(self.mod_job.iter().map(|(t, ..)| t))
    }

    /// Whether anything is still being installed into this instance, or its mods updated.
    pub fn busy_with(&self, inst_id: &str) -> bool {
        self.kinds.values().any(|k| k.installing.keys().any(|(id, _)| id == inst_id)) || self.updating(inst_id)
    }

    fn updating(&self, inst_id: &str) -> bool {
        self.mod_job.as_ref().is_some_and(|(.., id)| id == inst_id)
    }

    /// Run a mods update or rollback in the background; it toasts its own outcome.
    pub fn start_mod_job(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context, inst_id: &str, task: Task, fut: impl Future<Output = ModJobDone> + Send + 'static) {
        let tk = task.clone();
        let p = Promise::spawn(rt, ctx, async move {
            let r = fut.await;
            tk.finish(&if r.kind == ToastKind::Error { Err(anyhow::anyhow!("{}", r.text)) } else { Ok(()) });
            Ok(r)
        });
        self.mod_job = Some((task, p, inst_id.to_string()));
    }

    pub fn invalidate(&mut self) {
        for k in self.kinds.values_mut() {
            k.cache = None;
        }
    }

    pub fn poll(&mut self, toasts: &mut Vec<Toast>) {
        if let Some((_, Check::Running(p))) = &self.updates
            && let Some(r) = p.take()
        {
            match r {
                Ok(list) => {
                    if list.is_empty() {
                        toasts.push(Toast::info("Every mod is up to date"));
                    }
                    self.updates.as_mut().unwrap().1 = Check::Found(list);
                }
                Err(e) => {
                    toasts.push(Toast::error(format!("Couldn't check for updates: {e:#}")));
                    self.updates = None;
                }
            }
        }
        if let Some((_, p, _)) = self.mod_job.take_if(|(_, p, _)| p.ready()) {
            let done = p.take().unwrap().unwrap_or_else(|e| ModJobDone::new(ToastKind::Error, format!("{e:#}")));
            toasts.push(Toast { kind: done.kind, text: done.text, born: Instant::now() });
            self.world_prompts.extend(done.worlds);
            // What was found is out of date now.
            self.updates = None;
            self.invalidate();
        }
        for s in self.kinds.values_mut() {
            let meta = &self.meta;
            if let Some(p) = &s.search_p
                && let Some(r) = p.take()
            {
                s.results = Some(r.map(|s| s.hits).map_err(|e| format!("{e:#}")));
                s.result = None;
                s.search_p = None;
            }
            let done: Vec<(String, String)> = s.installing.iter().filter(|(_, j)| j.promise.ready()).map(|(k, _)| k.clone()).collect();
            for key in done {
                let Job { promise, title, dir, .. } = s.installing.remove(&key).unwrap();
                let (kind, text) = match promise.take().unwrap() {
                    Ok(done) if done.files.is_empty() && done.missing.is_empty() => (ToastKind::Info, format!("{title} is already installed")),
                    Ok(done) => {
                        meta.installed(&dir, &done);
                        let extra = done.files.len().saturating_sub(1);
                        let deps = if extra > 0 { format!(" (+{extra} dependencies)") } else { String::new() };
                        if done.missing.is_empty() {
                            (ToastKind::Info, format!("Installed {title}{deps}"))
                        } else {
                            // The mod won't load without these, so it's an error, not a success.
                            (ToastKind::Error, format!("Installed {title}{deps}, but not its required dependencies:\n{}", done.missing.join("\n")))
                        }
                    }
                    Err(e) => (ToastKind::Error, format!("{title}: {e:#}")),
                };
                toasts.push(Toast { kind, text, born: Instant::now() });
                s.cache = None;
            }
        }
    }
}

impl KindState {
    fn files(&mut self, inst: &Instance, kind: ContentKind) -> Vec<ModFile> {
        let fresh = self.cache.as_ref().is_some_and(|(id, _, at)| *id == inst.id && at.elapsed() < Duration::from_secs(2));
        if !fresh {
            self.cache = Some((inst.id.clone(), inst.content(kind), Instant::now()));
        }
        self.cache.as_ref().unwrap().1.clone()
    }

    fn search(&mut self, kind: ContentKind, inst: &Instance, rt: &tokio::runtime::Handle, ctx: &egui::Context) {
        let (i, q) = (inst.clone(), self.query.clone());
        // Keep the old results up while the query changes; only another instance's are stale.
        if self.searched.as_ref().is_none_or(|(id, _)| *id != inst.id) {
            self.results = None;
            self.result = None;
        }
        self.searched = Some((inst.id.clone(), q.clone()));
        self.typed = None;
        self.search_p = Some(Promise::spawn(rt, ctx, async move { modrinth::search(kind, &q, &i).await }));
    }
}

pub fn show(app: &mut App, ui: &mut Ui, inst: &Instance, kind: ContentKind) {
    let t = app.t.clone();
    let ctx = ui.ctx().clone();
    let has_loader = inst.loader.modrinth().is_some();
    if kind == ContentKind::Mods && !has_loader {
        w::empty_state(ui, &t, "󰏗", "Vanilla instances don't load mods.", "Pick a mod loader in the version tab.");
        return;
    }
    if kind == ContentKind::ShaderPacks {
        let iris = inst.mods().iter().any(|m| m.enabled && m.name.to_lowercase().starts_with("iris"));
        let note = if !has_loader {
            Some("Shaders need Iris, which needs a Fabric or Quilt instance.")
        } else if !iris {
            Some("Shaders need the Iris mod. Install it from the mods tab.")
        } else {
            None
        };
        if let Some(n) = note {
            w::para(ui, RichText::new(n).color(t.yellow));
            ui.add_space(4.0);
        }
    }

    let rt = app.rt.clone();
    let main_nav = app.nav_for(Zone::Main);
    let side_nav = app.nav_for(Zone::Side);
    let zone = app.focus;
    let focus = std::mem::take(&mut app.content.focus_search) || main_nav == Some(Nav::Search) || side_nav == Some(Nav::Search);
    let (meta, remote) = (app.content.meta.clone(), app.content.remote.clone());
    let s = app.content.kinds.entry(kind).or_default();
    if s.searched.as_ref().is_none_or(|(id, _)| *id != inst.id) {
        s.search(kind, inst, &rt, &ctx);
    }
    meta.ensure(&s.files(inst, kind), kind, &rt, &ctx);
    let mut out = Out::default();
    // Installing into a folder that's being moved into or out of a share set could land
    // the file in the old place or get it renamed on a clash.
    let blocked = if app.sharing.is_some() && crate::shares::SET_FOLDERS.contains(&kind.dir()) {
        Some("Shared folders are moving; install again in a moment")
    } else if kind == ContentKind::Mods && app.content.updating(&inst.id) {
        Some("Mods are updating; install again in a moment")
    } else {
        None
    };
    let ups = match kind {
        ContentKind::Mods => Some(Updates {
            list: match &app.content.updates {
                Some((id, Check::Found(l))) if *id == inst.id => l.clone(),
                _ => Vec::new(),
            },
            checking: app.content.updates.as_ref().is_some_and(|(id, c)| *id == inst.id && matches!(c, Check::Running(_))),
            running: app.content.updating(&inst.id),
            key: app.content.key.take(),
        }),
        _ => None,
    };
    let s = app.content.kinds.entry(kind).or_default();
    ui.columns(2, |cols| {
        installed(&mut cols[0], &t, s, &meta, inst, kind, main_nav, zone == Zone::Main, ups.as_ref(), &mut out);
        browse(&mut cols[1], &t, s, &meta, &remote, inst, kind, &rt, &ctx, focus, side_nav, zone == Zone::Side, blocked, &mut out);
    });
    if out.check {
        let i = inst.clone();
        app.content.updates = Some((inst.id.clone(), Check::Running(Promise::spawn(&rt, &ctx, async move { modrinth::check_updates(&i).await }))));
    }
    if let Some(list) = out.update {
        start_update(app, &ctx, inst, list);
    }
    if out.roll_back {
        open_roll_back(app, inst);
    }
    if focus || out.to_results {
        app.focus = Zone::Side;
    }
    if out.to_installed {
        app.focus = Zone::Main;
    }
    if let Some(e) = out.error {
        app.toast(ToastKind::Error, e);
    }
    if let Some(m) = out.remove {
        app.modal = Some(Modal::Confirm(Confirm {
            title: format!("remove {}", kind.noun()),
            body: format!("Remove {} from this instance?", m.display_name()),
            button: "remove".into(),
            action: ConfirmAction::DeletePath(m.path),
        }));
    }
}

/// What the panes ask of the app after drawing.
#[derive(Default)]
struct Out {
    remove: Option<ModFile>,
    error: Option<String>,
    to_results: bool,
    to_installed: bool,
    /// Check for mod updates, update these, or open the rollback dialog.
    check: bool,
    update: Option<Vec<Update>>,
    roll_back: bool,
}

/// The mods tab's update state, for the installed pane.
struct Updates {
    list: Vec<Update>,
    checking: bool,
    /// An update or rollback is running in this instance.
    running: bool,
    key: Option<Action>,
}

/// Update `list`'s mods in the background, unless the instance is busy.
fn start_update(app: &mut App, ctx: &egui::Context, inst: &Instance, list: Vec<Update>) {
    if let Some(why) = app.instance_busy(&inst.id) {
        app.toast(ToastKind::Error, format!("Can't update mods while {why}"));
        return;
    }
    let task = Task::new(format!("updating {}", inst.name));
    let (i, keep, tk) = (inst.clone(), app.cfg.backups_keep, task.clone());
    let rt = app.rt.clone();
    app.content.start_mod_job(&rt, ctx, &inst.id, task, async move {
        let (kind, text) = match modrinth::update(&i, list, keep, &tk).await {
            Ok(done) => {
                let mut text = match done.done.as_slice() {
                    [one] => format!("Updated {one}"),
                    many => format!("Updated {} mods:\n{}", many.len(), many.join("\n")),
                };
                if !done.added.is_empty() {
                    text.push_str(&format!("\nAdded {}", done.added.join(", ")));
                }
                if done.missing.is_empty() {
                    (ToastKind::Info, text)
                } else {
                    (ToastKind::Error, format!("{text}\nbut not these required dependencies:\n{}", done.missing.join("\n")))
                }
            }
            Err(e) => (ToastKind::Error, format!("{e:#}")),
        };
        ModJobDone::new(kind, text)
    });
}

/// The rollback dialog, listing the instance's backups of its mods and config.
fn open_roll_back(app: &mut App, inst: &Instance) {
    let list = crate::backups::instance_snapshots(&inst.id);
    app.modal = Some(Modal::RollBack(Box::new(RollBack { id: inst.id.clone(), name: inst.name.clone(), list, selected: 0 })));
}

pub struct RollBack {
    id: String,
    name: String,
    list: Vec<crate::backups::Snapshot>,
    selected: usize,
}

/// The rollback dialog. Returns true once it's done.
pub fn roll_back_dialog(app: &mut App, s: &mut RollBack, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let kb = app.modal_nav();
    ui.set_width(520.0);
    w::heading(ui, &t, "roll back mods");
    ui.add_space(8.0);
    if s.list.is_empty() {
        w::para(ui, RichText::new(format!("No backups of {}'s mods yet. One is taken before every update.", s.name)).color(t.muted));
        ui.add_space(14.0);
        return w::button(ui, "close").clicked() || kb == Some(Nav::Activate);
    }
    w::para(ui, RichText::new(format!("Put {}'s mods and config back as they were. What's there now is backed up first.", s.name)).color(t.muted));
    ui.add_space(8.0);
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(s.list.len(), Some(s.selected))) {
        s.selected = p;
    }
    let mut pick = kb == Some(Nav::Activate);
    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (n, snap) in s.list.iter().enumerate() {
            let on = n == s.selected;
            let r = w::rich_row(ui, &t, on, w::Lead::Glyph("\u{f006f}"), &snap.when(), &snap.info.reason, "");
            if on {
                w::focus_mark(ui, &t, r.rect);
            }
            if r.clicked() {
                s.selected = n;
            }
            if r.double_clicked() {
                pick = true;
            }
        }
    });
    ui.add_space(14.0);
    let mut done = false;
    ui.horizontal(|ui| {
        if w::primary_button(ui, &t, "roll back…").clicked() {
            pick = true;
        }
        done = w::button(ui, "cancel").clicked();
    });
    if pick && let Some(snap) = s.list.get(s.selected) {
        app.modal = Some(Modal::Confirm(Confirm {
            title: "roll back mods".into(),
            body: format!("Put {}'s mods and config back as they were on {}? What's there now is backed up first.", s.name, snap.when()),
            button: "roll back".into(),
            action: ConfirmAction::RollBack(s.id.clone(), snap.clone()),
        }));
        return true;
    }
    done
}

/// Roll `id`'s mods back to `snap` in the background (confirmed already).
pub fn start_roll_back(app: &mut App, ctx: &egui::Context, id: &str, snap: crate::backups::Snapshot) {
    let Some(inst) = app.instances.iter().find(|i| i.id == id).cloned() else { return };
    if let Some(why) = app.instance_busy(id) {
        app.toast(ToastKind::Error, format!("Can't roll back while {why}"));
        return;
    }
    let task = Task::new(format!("rolling back {}", inst.name));
    task.stage("Rolling back mods", 0);
    let keep = app.cfg.backups_keep;
    let rt = app.rt.clone();
    app.content.start_mod_job(&rt, ctx, id, task, async move {
        let when = snap.when();
        let r = tokio::task::spawn_blocking(move || {
            crate::backups::roll_back(&inst, &snap, keep)?;
            Ok::<_, anyhow::Error>(world_prompts(&inst, snap.info.taken))
        })
        .await;
        match r {
            Ok(Ok(worlds)) => ModJobDone { kind: ToastKind::Info, text: format!("Rolled mods back to {when}"), worlds },
            Ok(Err(e)) => ModJobDone::new(ToastKind::Error, format!("Couldn't roll back: {e:#}")),
            Err(e) => ModJobDone::new(ToastKind::Error, format!("Couldn't roll back: {e}")),
        }
    });
}

/// A world last played with mods a rollback just took away or made older: offer to
/// roll it back too.
pub struct WorldPrompt {
    inst_id: String,
    name: String,
    what: String,
    snaps: Vec<crate::backups::Snapshot>,
    selected: usize,
}

/// The worlds `inst` (just rolled back to mods from `before`) last played with newer
/// mods, each with its backups. The newest backup from before `before` is picked: the
/// world as it was before those mods were installed.
fn world_prompts(inst: &Instance, before: chrono::DateTime<chrono::Utc>) -> Vec<WorldPrompt> {
    let owner = crate::backups::Owner::of(inst);
    crate::world_sets::behind(inst)
        .into_iter()
        .map(|(w, d)| {
            let snaps = crate::backups::snapshots(owner, &w.folder);
            let selected = snaps.iter().position(|s| s.info.taken <= before).unwrap_or(0);
            WorldPrompt { inst_id: inst.id.clone(), name: w.name, what: d.describe(), snaps, selected }
        })
        .collect()
}

/// Asks whether to roll a world back too. Returns true once it's answered.
pub fn world_prompt_dialog(app: &mut App, p: &mut WorldPrompt, ui: &mut Ui) -> bool {
    let t = app.t.clone();
    let kb = app.modal_nav();
    ui.set_width(520.0);
    w::heading(ui, &t, "roll back the world too?");
    ui.add_space(8.0);
    w::para(ui, format!("“{}” was last played with {}, which the rollback took away. Restore the world from a backup as well?", p.name, p.what));
    ui.add_space(8.0);
    if p.snaps.is_empty() {
        w::para(ui, RichText::new("It has no backups to restore from.").color(t.muted));
        ui.add_space(14.0);
        return w::button(ui, "ok").clicked() || kb == Some(Nav::Activate);
    }
    if let Some(n) = kb.filter(|n| n.is_move()).and_then(|n| n.step(p.snaps.len(), Some(p.selected))) {
        p.selected = n;
    }
    let mut restore = kb == Some(Nav::Activate);
    egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (n, snap) in p.snaps.iter().enumerate() {
            let on = n == p.selected;
            let r = w::rich_row(ui, &t, on, w::Lead::Glyph("\u{f006f}"), &snap.when(), &snap.info.reason, if snap.info.auto { "auto" } else { "" });
            if on {
                w::focus_mark(ui, &t, r.rect);
            }
            if r.clicked() {
                p.selected = n;
            }
            if r.double_clicked() {
                restore = true;
            }
        }
    });
    ui.add_space(14.0);
    let mut no = false;
    ui.horizontal(|ui| {
        restore |= w::primary_button(ui, &t, "restore").clicked();
        no = w::button(ui, "no").clicked();
    });
    if restore && let Some(snap) = p.snaps.get(p.selected).cloned() {
        super::worlds::start_restore(app, ui.ctx(), &p.inst_id.clone(), snap);
        return true;
    }
    no
}

#[allow(clippy::too_many_arguments)]
fn installed(
    ui: &mut Ui,
    t: &crate::theme::Theme,
    s: &mut KindState,
    meta: &MetaStore,
    inst: &Instance,
    kind: ContentKind,
    kb: Option<Nav>,
    focused: bool,
    ups: Option<&Updates>,
    out: &mut Out,
) {
    let files = s.files(inst, kind);
    let update_for = |m: &ModFile| ups.and_then(|u| u.list.iter().find(|x| x.path == m.path));
    // Selection is by file stem, so it survives enable/disable renames.
    let stem = |m: &ModFile| m.name.trim_end_matches(".disabled").to_string();
    let pos = s.selected.as_ref().and_then(|n| files.iter().position(|m| stem(m) == *n));
    let mut scroll = false;
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(files.len(), pos)) {
        s.selected = Some(stem(&files[p]));
        scroll = true;
    }
    let key = |n: Nav| kb == Some(n);
    // Buttons, then the count: right-aligned if it fits, else on its own line.
    ui.horizontal_wrapped(|ui| {
        let sel = s.selected.as_ref().and_then(|n| files.iter().find(|m| stem(m) == *n)).cloned();
        if let Some(m) = &sel {
            if w::button(ui, if m.enabled { "disable" } else { "enable" }).clicked() || key(Nav::Toggle) || key(Nav::Activate) {
                if let Err(e) = m.toggle() {
                    out.error = Some(format!("{e:#}"));
                }
                s.cache = None;
            }
            if w::danger_button(ui, t, "remove").clicked() || key(Nav::Delete) {
                out.remove = Some(m.clone());
            }
            if let Some(u) = update_for(m)
                && !ups.is_some_and(|u| u.running)
                && (w::button(ui, "update").on_hover_text(format!("{} → {}", u.from, u.to.version_number)).clicked() || ups.is_some_and(|u| u.key == Some(Action::UpdateOne)))
            {
                out.update = Some(vec![u.clone()]);
            }
        }
        if w::button(ui, "folder").clicked() || key(Nav::Folder) {
            super::open_path(&inst.game_dir().join(kind.dir()));
        }
        if let Some(u) = ups {
            let key = u.key == Some(Action::Update);
            if u.running {
                ui.label(RichText::new("updating…").color(t.muted));
            } else if u.checking {
                ui.label(RichText::new("checking…").color(t.muted));
            } else if !u.list.is_empty() {
                if w::button(ui, &format!("update all ({})", u.list.len())).clicked() || key {
                    out.update = Some(u.list.clone());
                }
            } else if w::button(ui, "check updates").clicked() || key {
                out.check = true;
            }
            if (w::button(ui, "roll back…").clicked() || u.key == Some(Action::RollBack)) && !u.running {
                out.roll_back = true;
            }
        }
        let on = files.iter().filter(|f| f.enabled).count();
        let text = RichText::new(format!("{on}/{} enabled", files.len())).color(t.muted);
        let width = egui::WidgetText::from(text.clone()).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body).size().x;
        let room = ui.available_size_before_wrap().x;
        if room > width + 12.0 {
            ui.add_space(room - width - ui.spacing().item_spacing.x);
        } else {
            ui.end_row();
        }
        ui.label(text);
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical().id_salt(("installed", kind)).auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        if files.is_empty() {
            w::para(ui, RichText::new(format!("no {}s yet — search on the right", kind.noun())).color(t.muted));
        }
        ui.spacing_mut().item_spacing.y = 2.0;
        for m in &files {
            let sel = s.selected.as_deref() == Some(stem(m).as_str());
            let info = meta.get(m).unwrap_or_default();
            let lead = match &info.icon {
                Some(p) => w::Lead::Image(file_image(p)),
                None => w::Lead::Glyph(kind_glyph(kind)),
            };
            let title = info.name.clone().unwrap_or_else(|| m.display_name().to_string());
            let mut sub = Vec::new();
            if let Some(v) = &info.version {
                sub.push(v.clone());
            }
            sub.push(if m.is_dir { "folder".to_string() } else { w::size_label(m.size) });
            if title != m.display_name() {
                sub.push(m.display_name().to_string());
            }
            let mut right: Vec<String> = Vec::new();
            if update_for(m).is_some() {
                right.push("\u{f06b0} update".into());
            }
            if !m.enabled {
                right.push("disabled".into());
            }
            let r = w::rich_row_dim(ui, t, sel, lead, &title, &sub.join("  ·  "), &right.join("  ·  "), !m.enabled);
            if sel && focused {
                w::focus_mark(ui, t, r.rect);
            }
            if sel && scroll {
                r.scroll_to_me(None);
            }
            if r.clicked() {
                s.selected = Some(stem(m));
                out.to_installed = true;
            }
            if r.double_clicked() {
                if let Err(e) = m.toggle() {
                    out.error = Some(format!("{e:#}"));
                }
                s.cache = None;
            }
        }
    });
}

/// How long typing has to pause before the Modrinth search goes out.
const SEARCH_DELAY: Duration = Duration::from_millis(250);

#[allow(clippy::too_many_arguments)]
fn browse(
    ui: &mut Ui,
    t: &crate::theme::Theme,
    s: &mut KindState,
    meta: &MetaStore,
    remote: &RemoteIcons,
    inst: &Instance,
    kind: ContentKind,
    rt: &tokio::runtime::Handle,
    ctx: &egui::Context,
    focus: bool,
    kb: Option<Nav>,
    focused: bool,
    blocked: Option<&str>,
    out: &mut Out,
) {
    let r = w::text_input(ui, &mut s.query, &format!("/  search modrinth {}s", kind.noun()), ui.available_width(), focus);
    if r.changed() {
        s.typed = Some(Instant::now());
        ctx.request_repaint_after(SEARCH_DELAY);
    }
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let unsent = s.searched.as_ref().is_none_or(|(_, q)| *q != s.query);
    if unsent && (enter || s.typed.is_some_and(|at| at.elapsed() >= SEARCH_DELAY)) {
        s.search(kind, inst, rt, ctx);
    }
    if enter {
        out.to_results = true;
    }
    if s.search_p.is_some() && s.results.is_some() {
        let side = r.rect.height() - 10.0;
        let rect = egui::Rect::from_min_size(r.rect.right_top() + vec2(-side - 5.0, 5.0), vec2(side, side));
        egui::Spinner::new().color(t.muted).paint_at(ui, rect);
    }
    let count = s.results.as_ref().and_then(|r| r.as_ref().ok()).map_or(0, |h| h.len());
    let mut scroll = false;
    if let Some(p) = kb.filter(|n| n.is_move()).and_then(|n| n.step(count, s.result)) {
        s.result = Some(p);
        scroll = true;
    }
    let install_key = kb == Some(Nav::Activate);
    ui.add_space(6.0);
    let files = s.files(inst, kind);
    let installed: Vec<String> = files.iter().map(|m| m.display_name().to_lowercase()).collect();
    let installed_projects = meta.project_ids(&files);
    egui::ScrollArea::vertical().id_salt(("results", kind)).auto_shrink(false).show(ui, |ui| match &s.results {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("searching…").color(t.muted));
            });
        }
        Some(Err(e)) => {
            w::para(ui, RichText::new(e).color(t.red));
        }
        Some(Ok(hits)) if hits.is_empty() => {
            ui.label(RichText::new("no results").color(t.muted));
        }
        Some(Ok(hits)) => {
            for (n, h) in hits.clone().iter().enumerate() {
                let picked = s.result == Some(n);
                let job = (inst.id.clone(), h.project_id.clone());
                let have = installed_projects.contains(&h.project_id)
                    || installed.iter().any(|n| is_slug_file(n, &h.slug));
                let border = match (picked, focused) {
                    (true, true) => Stroke::new(1.0, t.accent),
                    (true, false) => Stroke::new(1.0, t.fg_a(0.5)),
                    _ => Stroke::new(1.0, t.fg_a(0.10)),
                };
                let card = egui::Frame::new().stroke(border).inner_margin(10).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        let size = t.body_size() * 3.0;
                        match h.icon_url.as_deref().and_then(|u| remote.get(u, rt, ctx)) {
                            Some(p) => {
                                ui.add(egui::Image::new(file_image(&p)).fit_to_exact_size(vec2(size, size)).corner_radius(6.0));
                            }
                            None => {
                                let (r, _) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
                                ui.painter().rect_filled(r, CornerRadius::same(6), t.fg_a(0.06));
                                let font = egui::FontId::monospace(t.body_size() * 1.4);
                                ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, kind_glyph(kind), font, t.muted);
                            }
                        }
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                // Right side first so long titles truncate instead of overlapping.
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if s.installing.contains_key(&job) {
                                        ui.spinner();
                                    } else if have {
                                        ui.label(RichText::new("installed").color(t.green));
                                    } else if w::button(ui, "install").clicked() || (picked && install_key) {
                                        if let Some(why) = blocked {
                                            out.error = Some(why.into());
                                        } else {
                                            let task = Task::new(format!("installing {}", h.title));
                                            task.stage(format!("Installing {}", h.title), 0);
                                            let (i, pid, tk) = (inst.clone(), h.project_id.clone(), task.clone());
                                            let p = Promise::spawn(rt, ctx, async move {
                                                let r = modrinth::install(kind, &pid, &i).await;
                                                tk.finish(&r.as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e:#}")));
                                                r
                                            });
                                            let dir = inst.game_dir().join(kind.dir());
                                            s.installing.insert(job.clone(), Job { task, promise: p, title: h.title.clone(), dir });
                                        }
                                    }
                                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                        let title = RichText::new(&h.title).font(crate::theme::bold(t.body_size())).color(t.fg_light);
                                        ui.add(egui::Label::new(title).truncate());
                                    });
                                });
                            });
                            let by = format!("by {}  ·  󰇚 {}", h.author, modrinth::format_count(h.downloads));
                            ui.add(egui::Label::new(RichText::new(by).color(t.muted)).truncate());
                            ui.add_space(2.0);
                            w::para(ui, RichText::new(&h.description).color(t.fg));
                        });
                    });
                });
                if picked && focused {
                    w::focus_mark(ui, t, card.response.rect);
                }
                if picked && scroll {
                    card.response.scroll_to_me(None);
                }
                ui.add_space(4.0);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mod jar with a fabric.mod.json, as `mods/<id>.jar`.
    fn jar(inst: &Instance, id: &str, version: &str) {
        use std::io::Write;
        let dir = inst.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(dir.join(format!("{id}.jar"))).unwrap());
        z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap();
        write!(z, r#"{{"schemaVersion": 1, "id": "{id}", "version": "{version}"}}"#).unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn a_rollback_offers_the_worlds_played_with_newer_mods() {
        use crate::backups::{self, Owner};
        let mut inst = Instance::new("prompt", "1.21.1", crate::loaders::Loader::Fabric("0.16.5".into()));
        inst.assign_id().unwrap();
        jar(&inst, "create", "6.0.0");
        let saves = inst.saves_dir();
        for w in ["Played", "Untouched"] {
            std::fs::create_dir_all(saves.join(w)).unwrap();
            std::fs::write(saves.join(w).join("level.dat"), "").unwrap();
        }
        let world = saves.join("Played");
        crate::world_sets::write_records(&inst, &[world.clone(), saves.join("Untouched")]);
        backups::snapshot_world(&world, Owner::of(&inst), "before the update", false).unwrap();

        // Update create, back up its mods first, then play the world with the new one.
        std::thread::sleep(Duration::from_millis(10));
        let mods = backups::snapshot_instance(&inst, "before updating create", 5).unwrap();
        jar(&inst, "create", "6.1.0");
        crate::world_sets::write_records(&inst, std::slice::from_ref(&world));
        std::thread::sleep(Duration::from_millis(10));
        backups::snapshot_world(&world, Owner::of(&inst), "after playing", true).unwrap();

        let snap = backups::instance_snapshots(&inst.id).into_iter().find(|s| s.dir == mods).unwrap();
        backups::roll_back(&inst, &snap, 5).unwrap();
        let prompts = world_prompts(&inst, snap.info.taken);
        assert_eq!(prompts.len(), 1, "only the world played with the newer create");
        let p = &prompts[0];
        assert_eq!(p.name, "Played");
        assert!(p.what.contains("create 6.1.0"), "{}", p.what);
        // The world as it was before the update is picked, not the newer one.
        assert_eq!(p.snaps.len(), 2);
        assert_eq!(p.snaps[p.selected].info.reason, "before the update");
        std::fs::remove_dir_all(inst.dir()).unwrap();
        backups::forget_instance(&inst.id);
    }

    fn file(dir: &std::path::Path, name: &str, size: u64, secs: u64) -> ModFile {
        let modified = Some(std::time::UNIX_EPOCH + Duration::from_secs(secs));
        ModFile { path: dir.join(name), name: name.into(), enabled: !name.ends_with(".disabled"), size, modified, is_dir: false }
    }

    #[test]
    fn slug_matches_whole_name_only() {
        for n in ["sodium", "sodium-0.6.0", "Sodium-fabric-0.5.8+mc1.20.1", "sodium_v1.2", "sodium-mc1.21-0.6", "sodium-neoforge-0.6.0"] {
            assert!(is_slug_file(n, "sodium"), "{n}");
        }
        for n in ["sodium-extra-0.5.4", "sodiumextra-1.0", "sodium-fabric", "reeses-sodium-options-1.7", "sodium-", "sod"] {
            assert!(!is_slug_file(n, "sodium"), "{n}");
        }
    }

    /// B8: "installed" comes from this instance's files as they are now, not from any instance ever seen.
    #[test]
    fn installed_projects_are_per_instance_and_current() {
        let (a, b) = (std::path::Path::new("/a/mods"), std::path::Path::new("/b/mods"));
        let meta = MetaStore::default();
        let sodium = file(a, "sodium.jar", 10, 1);
        let m = FileMeta { project_id: Some("AANobbMI".into()), ..Default::default() };
        meta.known.lock().unwrap().insert(meta_key(&sodium), m);

        assert!(meta.project_ids(std::slice::from_ref(&sodium)).contains("AANobbMI"));
        // Disabling keeps it; another instance, a removed file or a replacement (new mtime) don't.
        assert!(meta.project_ids(&[file(a, "sodium.jar.disabled", 10, 1)]).contains("AANobbMI"));
        assert!(meta.project_ids(&[file(b, "sodium.jar", 10, 1)]).is_empty());
        assert!(meta.project_ids(&[]).is_empty());
        assert!(meta.project_ids(&[file(a, "sodium.jar", 10, 2)]).is_empty());

        // Our own installs count before the hash lookup, in that instance only.
        let done = modrinth::Installed { files: vec!["lithium.jar".into()], projects: vec!["gvQqBUqZ".into()], missing: vec![] };
        meta.installed(a, &done);
        assert!(meta.project_ids(&[file(a, "lithium.jar", 5, 1)]).contains("gvQqBUqZ"));
        assert!(meta.project_ids(&[file(b, "lithium.jar", 5, 1)]).is_empty());
    }
}
