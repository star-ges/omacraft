//! The running game's output, and past logs and crash reports, with what a crash
//! analysis ([`crate::crash`]) makes of them.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use egui::{Align, Layout, RichText, Stroke, TextWrapMode, Ui};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, ToastKind};
use crate::crash::{Analysis, Confidence};
use crate::instance::Instance;
use crate::keymap::Action;
use crate::launch::{self, LogLevel, LogLine};
use crate::task::Promise;
use crate::theme::Theme;

/// A crashed game, glyph for crash reports and the crash notice.
const CRASH: &str = "\u{f015a}";
/// A cause that isn't a mod.
const ALERT: &str = "\u{f0026}";

/// Where the log view should go: a line (in the unfiltered log) to scroll to and mark.
#[derive(Clone, Copy, Default)]
pub struct Mark {
    pub line: Option<usize>,
    /// Scroll there this frame (once, after a click or key).
    pub jump: bool,
}

/// Scrolling, level-coloured log lines in a dark well.
/// `scroll` comes from the keyboard; `focused` outlines the well. `mark` is an index into `lines`.
pub fn render_lines(ui: &mut Ui, t: &Theme, lines: &[&LogLine], stick_to_bottom: bool, scroll: Option<Nav>, focused: bool, mark: Mark) {
    let row_h = t.body_size() + 4.0;
    let stroke = if focused { Stroke::new(1.0, t.accent) } else { Stroke::new(1.0, t.fg_a(0.10)) };
    egui::Frame::new().fill(t.bg_dark).stroke(stroke).inner_margin(8).show(ui, |ui| {
        let page = ui.available_height() * 0.9;
        let mut area = egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(stick_to_bottom);
        if let (Some(l), true) = (mark.line, mark.jump) {
            // A few lines of context above the marked one.
            area = area.vertical_scroll_offset((l.saturating_sub(4)) as f32 * row_h);
        }
        area.show_rows(ui, row_h, lines.len(), |ui, range| {
            if let Some(d) = scroll.and_then(|n| n.scroll_delta(row_h, page)) {
                ui.scroll_with_delta(egui::vec2(0.0, d));
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, line) in lines[range.clone()].iter().enumerate() {
                let color = match line.level {
                    LogLevel::Launcher => t.accent,
                    LogLevel::Info => t.fg,
                    LogLevel::Warn => t.yellow,
                    LogLevel::Error => t.red,
                    LogLevel::Debug => t.muted,
                };
                let text = RichText::new(line.text.replace('\t', "    ")).color(color).size(t.body_size() - 1.0);
                let r = ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), row_h), Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(text).wrap_mode(TextWrapMode::Truncate))
                });
                if mark.line == Some(range.start + i) {
                    let rect = r.response.rect.expand2(egui::vec2(8.0, 0.0));
                    ui.painter().rect_filled(rect, 0.0, t.fg_a(0.08));
                    w::focus_mark(ui, t, rect);
                }
            }
        });
    });
}

fn filtered<'a>(lines: impl Iterator<Item = &'a LogLine>, filter: &str) -> Vec<&'a LogLine> {
    let f = filter.trim().to_lowercase();
    lines.filter(|l| f.is_empty() || l.text.to_lowercase().contains(&f)).collect()
}

/// A game that crashed, for the notice in its log tab.
#[derive(Clone)]
pub struct Notice {
    /// The crash report, or the log the analysis read.
    pub path: PathBuf,
    pub headline: String,
}

pub fn live(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let why = app.logs.key.take() == Some(Action::Why);
    if let Some(n) = app.logs.notices.get(&inst.id).cloned() {
        let mut see = why;
        let mut dismiss = false;
        crash_banner(ui, &t, &n.headline, &mut see, &mut dismiss);
        if see {
            app.logs.show_analysis(&n.path);
            app.inst_tab = super::instances::InstTab::LogHistory;
            app.focus = Zone::Side;
        }
        if dismiss {
            app.logs.notices.remove(&inst.id);
        }
        ui.add_space(8.0);
    }
    let Some(session) = app.sessions.get(&inst.id) else {
        w::empty_state(ui, &t, "\u{f018d}", "not running", "Press play to see the game's output here. Earlier runs are in log history.");
        return;
    };
    let log = session.log.clone();
    let lines = log.lock().unwrap().lines.clone();
    let kb = app.nav_for(Zone::Main);
    let focused = app.focus == Zone::Main;
    let mut copy = kb == Some(Nav::Copy);
    ui.horizontal(|ui| {
        w::text_input(ui, &mut app.logs.live_filter, "filter", 220.0, kb == Some(Nav::Search));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            copy |= w::button(ui, "copy").clicked();
            if w::button(ui, "clear").clicked() {
                log.lock().unwrap().lines.clear();
            }
            ui.label(RichText::new(format!("{} lines", lines.len())).color(t.muted));
        });
    });
    ui.add_space(4.0);
    let shown = filtered(lines.iter(), &app.logs.live_filter);
    if copy {
        ui.ctx().copy_text(shown.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"));
        app.toast(ToastKind::Info, "Log copied");
    }
    render_lines(ui, &t, &shown, true, kb, focused, Mark::default());
}

/// "game crashed · likely cause: Sodium 0.5.8   [see why] [dismiss]"
fn crash_banner(ui: &mut Ui, t: &Theme, headline: &str, see: &mut bool, dismiss: &mut bool) {
    egui::Frame::new().fill(Theme::alpha(t.red, 0.06)).stroke(Stroke::new(1.0, Theme::alpha(t.red, 0.45))).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                *dismiss |= w::button(ui, "dismiss").clicked();
                *see |= w::button(ui, "see why").clicked();
                ui.add_space(8.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new(CRASH).color(t.red));
                    ui.label(RichText::new("game crashed").color(t.fg_light));
                    ui.label(RichText::new("·").color(t.muted));
                    ui.add(egui::Label::new(RichText::new(headline).color(t.fg)).truncate());
                });
            });
        });
    });
}

// -- history --------------------------------------------------------------------

#[derive(Clone)]
struct LogFile {
    path: PathBuf,
    name: String,
    modified: Option<SystemTime>,
    size: u64,
    crash: bool,
}

/// A log read from disk, with an analysis if it shows a crash.
struct Loaded {
    lines: Vec<LogLine>,
    analysis: Option<Analysis>,
}

/// The analysis panel's own state.
#[derive(Default)]
struct Panel {
    /// The keyboard drives the panel rather than the log under it.
    focused: bool,
    selected: usize,
    /// Every suspect, not just the first few.
    all: bool,
    hidden: bool,
}

/// Suspects listed before "n more".
const SHOWN: usize = 4;

#[derive(Default)]
pub struct LogsState {
    pub live_filter: String,
    files: Option<(String, Vec<LogFile>, Instant)>,
    selected: Option<PathBuf>,
    loaded: Option<(PathBuf, Result<Loaded, String>)>,
    load_p: Option<(PathBuf, Promise<Loaded>)>,
    filter: String,
    panel: Panel,
    mark: Mark,
    /// sha1 → Modrinth project id (None: not on Modrinth), for "modrinth" buttons.
    pages: HashMap<String, Option<String>>,
    page_p: Option<(String, Promise<HashMap<String, crate::content_meta::Project>>)>,
    /// An instance action for these tabs (`why`), taken by whichever is drawn.
    pub key: Option<Action>,
    /// Instance id → its last game's crash.
    pub notices: HashMap<String, Notice>,
    /// Games that just ended, being checked for a crash: (instance id, exit, result).
    crash_p: Vec<(String, Result<i32, String>, Promise<Option<Notice>>)>,
}

impl LogsState {
    /// Open the history on `path`, the analysis in front.
    pub fn show_analysis(&mut self, path: &Path) {
        self.selected = Some(path.to_path_buf());
        self.filter.clear();
        self.panel = Panel { focused: true, ..Default::default() };
        self.mark = Mark::default();
        // Re-read: the file may be newer than what's loaded.
        self.loaded = None;
        self.load_p = None;
        self.files = None;
    }

    /// The analysis of the log on screen, if it shows a crash.
    pub fn analysis(&self) -> Option<&Analysis> {
        match &self.loaded {
            Some((p, Ok(l))) if Some(p) == self.selected.as_ref() => l.analysis.as_ref().filter(|a| a.crashed),
            _ => None,
        }
    }

    /// Whether the keyboard is on the analysis panel (in log history's right pane).
    pub fn panel_focused(&self) -> bool {
        self.panel.focused && !self.panel.hidden && self.analysis().is_some()
    }

    /// Check a game that just ended for a crash: a crash report written since it
    /// started, or else a log that shows one. A game the user stopped isn't checked.
    pub fn watch_exit(&mut self, rt: &tokio::runtime::Handle, ctx: &egui::Context, inst: &Instance, default_memory_mb: u32, started: SystemTime, exit: Result<i32, String>, lines: Vec<String>) {
        if exit.as_ref().is_err_and(|e| e == "killed by signal") {
            return;
        }
        let (id, inst, failed) = (inst.id.clone(), inst.clone(), exit != Ok(0));
        let p = Promise::spawn(rt, ctx, async move {
            Ok(tokio::task::spawn_blocking(move || {
                let (path, text) = match crate::crash::crash_report_since(&inst, started) {
                    Some(p) => {
                        let text = String::from_utf8_lossy(&crate::fsutil::read_capped(std::fs::File::open(&p).ok()?).ok()?).into_owned();
                        (p, text)
                    }
                    None if failed => (inst.logs_dir().join("latest.log"), lines.join("\n")),
                    None => return None,
                };
                let a = crate::crash::analyze(&inst, default_memory_mb, &text);
                (a.crashed || failed).then(|| Notice { path, headline: if a.crashed { a.headline() } else { "see the log".into() } })
            })
            .await?)
        });
        self.crash_p.push((id, exit, p));
    }
}

/// Poll the crash checks of games that ended; toast what they found.
pub fn poll_crashes(app: &mut App) {
    let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut app.logs.crash_p).into_iter().partition(|(.., p)| p.ready());
    app.logs.crash_p = running;
    for (id, exit, p) in done {
        match p.take().and_then(|r| r.ok()).flatten() {
            Some(n) => {
                let name = app.instances.iter().find(|i| i.id == id).map(|i| i.name.clone()).unwrap_or_default();
                app.toast(ToastKind::Error, format!("{name} crashed: {}. The log tab says why", n.headline));
                app.logs.notices.insert(id, n);
            }
            None => match exit {
                Ok(0) => {}
                Ok(c) => app.toast(ToastKind::Error, format!("Minecraft exited with code {c}; see the log")),
                Err(e) => app.toast(ToastKind::Error, format!("Minecraft {e}")),
            },
        }
    }
}

fn list_files(inst: &Instance) -> Vec<LogFile> {
    let mut out = Vec::new();
    for (dir, crash) in [(inst.logs_dir(), false), (inst.crash_reports_dir(), true)] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let ok = if crash { name.ends_with(".txt") } else { name.ends_with(".log") || name.ends_with(".log.gz") };
            let Ok(meta) = e.metadata() else { continue };
            if ok && meta.is_file() {
                out.push(LogFile { path: e.path(), name, modified: meta.modified().ok(), size: meta.len(), crash });
            }
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

fn read_log(path: &Path) -> anyhow::Result<(String, bool)> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    if path.extension().is_some_and(|e| e == "gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
        bytes = out;
    }
    let crash = path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "crash-reports");
    Ok((String::from_utf8_lossy(&bytes).into_owned(), crash))
}

/// Read a log and, if it shows a crash, analyse it (reads the instance's jars).
fn load(path: &Path, inst: &Instance, default_memory_mb: u32) -> anyhow::Result<Loaded> {
    let (text, crash) = read_log(path)?;
    let lines = text.lines().map(|l| LogLine { level: if crash { LogLevel::Error } else { launch::classify(l, false) }, text: l.to_string() }).collect();
    let analysis = (crash || crate::crash::looks_crashed(&text)).then(|| crate::crash::analyze(inst, default_memory_mb, &text));
    Ok(Loaded { lines, analysis })
}

fn modified_label(m: Option<SystemTime>) -> String {
    m.map(|m| w::ago(Some(chrono::DateTime::<chrono::Utc>::from(m)))).unwrap_or_default()
}

pub fn history(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let mem = app.cfg.default_memory_mb;
    // Mods can't be switched while they're being updated, or as the game starts.
    let mods_busy = if app.content.updating(&inst.id) {
        Some("mods are being updated")
    } else if app.sessions.get(&inst.id).is_some_and(|s| s.starting()) {
        Some("the game is starting")
    } else {
        None
    };
    let why = app.logs.key.take() == Some(Action::Why);
    let s = &mut app.logs;
    let fresh = s.files.as_ref().is_some_and(|(id, _, at)| *id == inst.id && at.elapsed() < Duration::from_secs(3));
    if !fresh {
        if s.files.as_ref().is_some_and(|(id, _, _)| *id != inst.id) {
            s.selected = None;
            s.loaded = None;
            s.panel = Panel::default();
            s.mark = Mark::default();
        }
        s.files = Some((inst.id.clone(), list_files(inst), Instant::now()));
    }
    let files = s.files.as_ref().unwrap().1.clone();
    let list_nav = app.nav_for(Zone::Main);
    let view_nav = app.nav_for(Zone::Side);
    let focus = app.focus;
    let s = &mut app.logs;
    if files.is_empty() {
        w::empty_state(ui, &t, "\u{f0219}", "no logs yet", "Minecraft writes logs and crash reports here after it runs.");
        return;
    }
    if s.selected.is_none() {
        s.selected = files.first().map(|f| f.path.clone());
    }
    if let Some((path, p)) = &s.load_p
        && let Some(r) = p.take()
    {
        s.loaded = Some((path.clone(), r.map_err(|e| format!("{e:#}"))));
        s.load_p = None;
    }
    let mut scroll = false;
    if let Some(n) = list_nav.filter(|n| n.is_move()) {
        let pos = files.iter().position(|f| Some(&f.path) == s.selected.as_ref());
        if let Some(p) = n.step(files.len(), pos) {
            s.selected = Some(files[p].path.clone());
            scroll = true;
        }
    }
    let sel = s.selected.clone().unwrap();
    let have = s.loaded.as_ref().is_some_and(|(p, _)| *p == sel) || s.load_p.as_ref().is_some_and(|(p, _)| *p == sel);
    if !have {
        let (path, i) = (sel.clone(), inst.clone());
        s.load_p = Some((sel.clone(), Promise::spawn(&app.rt, ui.ctx(), async move { tokio::task::spawn_blocking(move || load(&path, &i, mem)).await? })));
        s.mark = Mark::default();
        s.panel.selected = 0;
        s.panel.all = false;
    }
    // `why` turns the keyboard to the analysis and back, showing it if hidden.
    let has_analysis = s.analysis().is_some();
    if why && has_analysis {
        if s.panel.hidden || !s.panel.focused || focus != Zone::Side {
            s.panel.hidden = false;
            s.panel.focused = true;
            app.focus = Zone::Side;
        } else {
            s.panel.focused = false;
        }
    }
    let focus = app.focus;
    let s = &mut app.logs;
    let panel_keys = s.panel_focused() && focus == Zone::Side;
    let (panel_nav, view_nav) = if panel_keys { (view_nav, None) } else { (None, view_nav) };
    let key = |n: Nav| list_nav == Some(n) || view_nav == Some(n);

    let mut clicked = false;
    egui::Panel::left("log-files").exact_size(260.0).resizable(false).frame(egui::Frame::new().inner_margin(egui::Margin { right: 12, ..Default::default() })).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("log-files").auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for f in &files {
                let on = f.path == sel;
                let sub = format!("{}  ·  {}", modified_label(f.modified), w::size_label(f.size));
                let glyph = if f.crash { CRASH } else { "\u{f0219}" };
                let r = w::rich_row(ui, &t, on, w::Lead::Glyph(glyph), &f.name, &sub, "");
                if on && focus == Zone::Main {
                    w::focus_mark(ui, &t, r.rect);
                }
                if on && scroll {
                    r.scroll_to_me(None);
                }
                if r.clicked() {
                    s.selected = Some(f.path.clone());
                    clicked = true;
                }
            }
        });
    });

    let mut copy = key(Nav::Copy);
    ui.horizontal(|ui| {
        w::text_input(ui, &mut s.filter, "filter", 220.0, key(Nav::Search));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            copy |= w::button(ui, "copy").clicked();
            if w::button(ui, "open").clicked() || key(Nav::Activate) {
                super::open_url(&sel.display().to_string());
            }
            if w::button(ui, "folder").clicked() || key(Nav::Folder) {
                super::open_path(sel.parent().unwrap());
            }
            if has_analysis && s.panel.hidden && w::button(ui, "why it crashed").clicked() {
                s.panel.hidden = false;
            }
        });
    });
    ui.add_space(6.0);

    let mut out = PanelOut::default();
    if let Some(a) = s.analysis().cloned().filter(|_| !s.panel.hidden) {
        // The selected suspect's Modrinth page, looked up once by hash.
        let sel_sha = a.suspects.get(s.panel.selected).and_then(|x| x.sha1.clone());
        if let Some((sha, p)) = &s.page_p
            && let Some(r) = p.take()
        {
            let found = r.ok().and_then(|mut m| m.remove(sha)).map(|p| p.id);
            s.pages.insert(sha.clone(), found);
            s.page_p = None;
        }
        if let Some(sha) = sel_sha.filter(|h| !s.pages.contains_key(h))
            && s.page_p.is_none()
        {
            let h = sha.clone();
            s.page_p = Some((sha, Promise::spawn(&app.rt, ui.ctx(), crate::content_meta::lookup(vec![h]))));
        }
        out = panel(ui, &t, &a, &mut s.panel, panel_nav, panel_keys, &s.pages, mods_busy);
        ui.add_space(8.0);
    }
    if let Some(line) = out.jump {
        s.filter.clear();
        s.mark = Mark { line: Some(line), jump: true };
    }
    let mark = s.mark;
    s.mark.jump = false;
    match &s.loaded {
        Some((p, Ok(l))) if *p == sel => {
            let shown = filtered(l.lines.iter(), &s.filter);
            if copy {
                ui.ctx().copy_text(shown.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"));
            }
            // A mark is a line of the whole log: only meaningful unfiltered.
            let mark = if s.filter.trim().is_empty() { mark } else { Mark::default() };
            render_lines(ui, &t, &shown, false, view_nav, focus == Zone::Side && !panel_keys, mark);
        }
        Some((p, Err(e))) if *p == sel => {
            w::para(ui, RichText::new(e).color(t.red));
        }
        _ => {
            ui.spinner();
        }
    }
    if copy {
        app.toast(ToastKind::Info, "Log copied");
    }
    if clicked {
        app.focus = Zone::Main;
    }
    let a = app.logs.analysis().cloned();
    if let Some(a) = a {
        act(app, ui.ctx(), inst, &a, out);
    }
}

/// What the panel asked for this frame.
#[derive(Default)]
struct PanelOut {
    jump: Option<usize>,
    toggle: Option<usize>,
    page: Option<String>,
    copy: bool,
    bisect: bool,
}

fn act(app: &mut App, ctx: &egui::Context, inst: &Instance, a: &Analysis, out: PanelOut) {
    if out.copy {
        ctx.copy_text(a.summary());
        app.toast(ToastKind::Info, "Crash summary copied");
    }
    if let Some(id) = out.page {
        super::open_url(&format!("https://modrinth.com/project/{id}"));
    }
    if out.bisect {
        start_bisect(app, inst, a);
    }
    if let Some(i) = out.toggle
        && let Some(s) = a.suspects.get(i)
        && let Some(file) = &s.file
    {
        let Some(m) = current_file(inst, file) else {
            app.toast(ToastKind::Error, format!("{} isn't in the mods folder any more", s.name));
            return;
        };
        match m.toggle() {
            Ok(()) => {
                app.content.invalidate();
                let what = if m.enabled { "Disabled" } else { "Enabled" };
                let next = if m.enabled { "; play to see if the crash is gone" } else { "" };
                app.toast(ToastKind::Info, format!("{what} {}{next}", s.name));
            }
            Err(e) => app.toast(ToastKind::Error, format!("{e:#}")),
        }
    }
}

/// Hook for the bisect: start finding the breaking mod among `a`'s suspects first.
// TODO(merge): wire to the bisect module, then set BISECT to true.
fn start_bisect(_app: &mut App, _inst: &Instance, _a: &Analysis) {}
const BISECT: bool = false;

/// The mod file the analysis named, as it is now (it may have been enabled or disabled since).
fn current_file(inst: &Instance, file: &Path) -> Option<crate::instance::ModFile> {
    let base = file.file_name()?.to_string_lossy().trim_end_matches(".disabled").to_string();
    inst.mods().into_iter().find(|m| m.name.trim_end_matches(".disabled") == base)
}

/// Whether the mod file is enabled now: Some(true/false), None if it's gone.
fn enabled_now(file: &Path) -> Option<bool> {
    let name = file.file_name()?.to_string_lossy().trim_end_matches(".disabled").to_string();
    let on = file.with_file_name(&name);
    if on.exists() {
        Some(true)
    } else if file.with_file_name(format!("{name}.disabled")).exists() {
        Some(false)
    } else {
        None
    }
}

fn confidence_color(t: &Theme, c: Confidence) -> egui::Color32 {
    match c {
        Confidence::Certain => t.red,
        Confidence::Likely => t.yellow,
        Confidence::Possible => t.muted,
    }
}

/// The analysis: a headline, causes that aren't mods, then the suspects with their
/// reasons; the selected suspect shows its evidence and actions.
#[allow(clippy::too_many_arguments)]
fn panel(ui: &mut Ui, t: &Theme, a: &Analysis, st: &mut Panel, kb: Option<Nav>, focused: bool, pages: &HashMap<String, Option<String>>, mods_busy: Option<&str>) -> PanelOut {
    let mut out = PanelOut::default();
    let n = a.suspects.len();
    let shown = if st.all { n } else { n.min(SHOWN) };
    if st.selected >= n.max(1) {
        st.selected = n.saturating_sub(1);
    }
    let mut scroll_sel = false;
    if let Some(nav) = kb {
        if let Some(p) = Some(nav).filter(|m| m.is_move()).and_then(|m| m.step(n, Some(st.selected))) {
            st.selected = p;
            st.all |= p >= SHOWN;
            scroll_sel = true;
        }
        match nav {
            Nav::Activate => out.jump = a.suspects.get(st.selected).and_then(|s| s.evidence.first()).map(|e| e.line),
            Nav::Toggle if mods_busy.is_none() => out.toggle = Some(st.selected).filter(|&i| a.suspects.get(i).is_some_and(|s| s.file.is_some())),
            Nav::Copy => out.copy = true,
            _ => {}
        }
    }
    let top = a.suspects.first().map(|s| s.confidence);
    let tone = match (top, a.causes.is_empty()) {
        (Some(Confidence::Certain), _) | (_, false) => t.red,
        (Some(Confidence::Likely), _) => t.yellow,
        _ => t.muted,
    };
    let border = if focused { t.accent } else { Theme::alpha(tone, 0.45) };
    let max_h = (ui.available_height() * 0.5).max(160.0);
    egui::Frame::new().fill(t.fg_a(0.025)).stroke(Stroke::new(1.0, border)).inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Headline, with the panel's actions laid out first so it never runs under them.
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, "hide").clicked() {
                    st.hidden = true;
                }
                out.copy |= w::button(ui, "copy summary").clicked();
                if BISECT && w::button(ui, "find it by bisecting").clicked() {
                    out.bisect = true;
                }
                ui.add_space(8.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new(CRASH).color(tone).size(t.body_size() + 2.0));
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(RichText::new(a.headline()).font(crate::theme::bold(t.body_size() + 1.0)).color(t.fg_light)).truncate());
                });
            });
        });
        let mut sub = Vec::new();
        if let Some(d) = &a.description {
            sub.push(format!("minecraft says: {}", d.to_lowercase()));
        }
        match n {
            0 if a.causes.is_empty() => sub.push("nothing in this log points at a mod; finding it by elimination is the way".into()),
            0 => {}
            1 => sub.push("1 suspect".into()),
            _ => sub.push(format!("{n} suspects")),
        }
        if !sub.is_empty() {
            ui.add(egui::Label::new(RichText::new(sub.join("  ·  ")).color(t.muted)).truncate());
        }
        if a.causes.is_empty() && n == 0 {
            return;
        }
        ui.add_space(10.0);
        egui::ScrollArea::vertical().id_salt("crash-suspects").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let tag_w = t.body_size() * 6.5;
            let headline = a.headline();
            for c in &a.causes {
                ui.horizontal_top(|ui| {
                    tag(ui, t, tag_w, ALERT, "cause", t.orange);
                    ui.vertical(|ui| {
                        // The headline may already name it.
                        if c.title != headline {
                            ui.label(RichText::new(&c.title).font(crate::theme::bold(t.body_size())).color(t.fg_light));
                        }
                        w::para(ui, RichText::new(&c.advice).color(t.fg));
                        for e in &c.evidence {
                            if evidence_row(ui, t, e) {
                                out.jump = Some(e.line);
                            }
                        }
                    });
                });
                ui.add_space(8.0);
            }
            for (i, s) in a.suspects.iter().enumerate().take(shown) {
                let on = i == st.selected;
                let r = suspect_row(ui, t, tag_w, s, on);
                if on && focused {
                    w::focus_mark(ui, t, r.rect);
                }
                if on && scroll_sel {
                    r.scroll_to_me(None);
                }
                if r.clicked() {
                    st.selected = i;
                }
                if on {
                    ui.horizontal(|ui| {
                        ui.add_space(tag_w + 8.0);
                        ui.vertical(|ui| {
                            for e in &s.evidence {
                                if evidence_row(ui, t, e) {
                                    out.jump = Some(e.line);
                                }
                            }
                            if s.evidence.is_empty() {
                                ui.label(RichText::new("from the jar itself, not the log").color(t.muted).size(t.body_size() - 1.0));
                            }
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if let Some(file) = &s.file {
                                    let now = enabled_now(file);
                                    let label = if now == Some(false) { "enable" } else { "disable" };
                                    let b = ui.add_enabled(mods_busy.is_none() && now.is_some(), egui::Button::new(label));
                                    let b = match (mods_busy, now) {
                                        (Some(why), _) => b.on_disabled_hover_text(format!("not while {why}")),
                                        (_, None) => b.on_disabled_hover_text("it's not in the mods folder any more"),
                                        _ => b,
                                    };
                                    if b.clicked() {
                                        out.toggle = Some(i);
                                    }
                                }
                                if let Some(Some(id)) = s.sha1.as_ref().and_then(|h| pages.get(h))
                                    && w::button(ui, "modrinth").clicked()
                                {
                                    out.page = Some(id.clone());
                                }
                            });
                            ui.add_space(6.0);
                        });
                    });
                }
            }
            if n > shown && ui.add(egui::Label::new(RichText::new(format!("{} more, less likely", n - shown)).color(t.muted)).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                st.all = true;
            }
        });
    });
    out
}

/// A fixed-width confidence tag: "certain", "likely", "cause".
fn tag(ui: &mut Ui, t: &Theme, width: f32, glyph: &str, label: &str, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, t.body_size() + 6.0), egui::Sense::hover());
    let font = egui::FontId::monospace(t.body_size() - 1.0);
    let text = if glyph.is_empty() { label.to_string() } else { format!("{glyph} {label}") };
    ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER, text, font, color);
}

/// One suspect: confidence, name and version, and the reason, on one clickable row.
fn suspect_row(ui: &mut Ui, t: &Theme, tag_w: f32, s: &crate::crash::Suspect, on: bool) -> egui::Response {
    let h = t.body_size() + 14.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if on {
            p.rect(rect, 0.0, t.fg_a(t.menu_selected_bg_alpha), Stroke::new(1.0, t.fg_a(t.menu_selected_border_alpha)), egui::StrokeKind::Inside);
        } else if resp.hovered() {
            p.rect_filled(rect, 0.0, t.fg_a(t.hover_fill_alpha * 0.6));
        }
        let size = t.body_size();
        let cy = rect.center().y;
        let x0 = rect.left() + 8.0;
        p.text(egui::pos2(x0, cy), egui::Align2::LEFT_CENTER, s.confidence.label(), egui::FontId::monospace(size - 1.0), confidence_color(t, s.confidence));
        let name = p.layout_no_wrap(s.label(), crate::theme::bold(size), t.fg_light);
        let nx = x0 + tag_w;
        let name_w = name.size().x.min(rect.width() * 0.45);
        let clip = egui::Rect::from_min_max(egui::pos2(nx, rect.top()), egui::pos2(nx + name_w, rect.bottom()));
        p.with_clip_rect(clip).galley(egui::pos2(nx, cy - name.size().y / 2.0), name, t.fg_light);
        let rx = nx + name_w + 14.0;
        // The reason gets what's left of the row, cut with an ellipsis.
        let mut job = egui::text::LayoutJob::single_section(s.reason.clone(), egui::TextFormat { font_id: egui::FontId::monospace(size - 1.0), color: t.fg, ..Default::default() });
        job.wrap = egui::text::TextWrapping { max_width: (rect.right() - 8.0 - rx).max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let reason = p.layout_job(job);
        p.galley(egui::pos2(rx, cy - reason.size().y / 2.0), reason, t.fg);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A quoted log line with its number; clicking shows it in the log. Returns whether clicked.
fn evidence_row(ui: &mut Ui, t: &Theme, e: &crate::crash::Evidence) -> bool {
    let size = t.body_size() - 1.0;
    let mut job = egui::text::LayoutJob::default();
    let fmt = |c| egui::TextFormat { font_id: egui::FontId::monospace(size), color: c, ..Default::default() };
    job.append(&format!("{:>6}  ", e.line + 1), 0.0, fmt(t.muted));
    job.append(&e.text, 0.0, fmt(t.fg_a(0.75)));
    let r = ui.add(egui::Label::new(job).truncate().sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("show in the log");
    if r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom(), Stroke::new(1.0, t.fg_a(0.25)));
    }
    r.clicked()
}
