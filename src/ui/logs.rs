//! The running game's output, and past logs and crash reports.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use egui::{Align, Layout, RichText, Stroke, TextWrapMode, Ui};

use super::widgets as w;
use super::nav::{Nav, Zone};
use super::{App, ToastKind};
use crate::instance::Instance;
use crate::launch::{self, LogLevel, LogLine};
use crate::task::Promise;
use crate::theme::Theme;

/// Scrolling, level-coloured log lines in a dark well.
/// `scroll` comes from the keyboard; `focused` outlines the well.
pub fn render_lines(ui: &mut Ui, t: &Theme, lines: &[&LogLine], stick_to_bottom: bool, scroll: Option<Nav>, focused: bool) {
    let row_h = t.body_size() + 4.0;
    let stroke = if focused { Stroke::new(1.0, t.accent) } else { Stroke::new(1.0, t.fg_a(0.10)) };
    egui::Frame::new().fill(t.bg_dark).stroke(stroke).inner_margin(8).show(ui, |ui| {
        let page = ui.available_height() * 0.9;
        egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(stick_to_bottom).show_rows(ui, row_h, lines.len(), |ui, range| {
            if let Some(d) = scroll.and_then(|n| n.scroll_delta(row_h, page)) {
                ui.scroll_with_delta(egui::vec2(0.0, d));
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            for line in &lines[range] {
                let color = match line.level {
                    LogLevel::Launcher => t.accent,
                    LogLevel::Info => t.fg,
                    LogLevel::Warn => t.yellow,
                    LogLevel::Error => t.red,
                    LogLevel::Debug => t.muted,
                };
                let text = RichText::new(line.text.replace('\t', "    ")).color(color).size(t.body_size() - 1.0);
                ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), row_h), Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(text).wrap_mode(TextWrapMode::Truncate))
                });
            }
        });
    });
}

fn filtered<'a>(lines: impl Iterator<Item = &'a LogLine>, filter: &str) -> Vec<&'a LogLine> {
    let f = filter.trim().to_lowercase();
    lines.filter(|l| f.is_empty() || l.text.to_lowercase().contains(&f)).collect()
}

pub fn live(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let Some(session) = app.sessions.get(&inst.id) else {
        w::empty_state(ui, &t, "󰆍", "not running", "Press play to see the game's output here. Earlier runs are in log history.");
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
    render_lines(ui, &t, &shown, true, kb, focused);
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

#[derive(Default)]
pub struct LogsState {
    pub live_filter: String,
    files: Option<(String, Vec<LogFile>, Instant)>,
    selected: Option<PathBuf>,
    loaded: Option<(PathBuf, Result<Vec<LogLine>, String>)>,
    load_p: Option<(PathBuf, Promise<Vec<LogLine>>)>,
    filter: String,
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

fn read_log(path: PathBuf) -> anyhow::Result<Vec<LogLine>> {
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?.read_to_end(&mut bytes)?;
    if path.extension().is_some_and(|e| e == "gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
        bytes = out;
    }
    let crash = path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "crash-reports");
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(|l| LogLine { level: if crash { LogLevel::Error } else { launch::classify(l, false) }, text: l.to_string() })
        .collect())
}

fn modified_label(m: Option<SystemTime>) -> String {
    m.map(|m| w::ago(Some(chrono::DateTime::<chrono::Utc>::from(m)))).unwrap_or_default()
}

pub fn history(app: &mut App, ui: &mut Ui, inst: &Instance) {
    let t = app.t.clone();
    let s = &mut app.logs;
    let fresh = s.files.as_ref().is_some_and(|(id, _, at)| *id == inst.id && at.elapsed() < Duration::from_secs(3));
    if !fresh {
        if s.files.as_ref().is_some_and(|(id, _, _)| *id != inst.id) {
            s.selected = None;
            s.loaded = None;
        }
        s.files = Some((inst.id.clone(), list_files(inst), Instant::now()));
    }
    let files = s.files.as_ref().unwrap().1.clone();
    let list_nav = app.nav_for(Zone::Main);
    let view_nav = app.nav_for(Zone::Side);
    let focus = app.focus;
    let s = &mut app.logs;
    if files.is_empty() {
        w::empty_state(ui, &t, "󰈙", "no logs yet", "Minecraft writes logs and crash reports here after it runs.");
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
    let key = |n: Nav| list_nav == Some(n) || view_nav == Some(n);
    let have = s.loaded.as_ref().is_some_and(|(p, _)| *p == sel) || s.load_p.as_ref().is_some_and(|(p, _)| *p == sel);
    if !have {
        let path = sel.clone();
        s.load_p = Some((sel.clone(), Promise::spawn(&app.rt, ui.ctx(), async move { tokio::task::spawn_blocking(move || read_log(path)).await? })));
    }

    let mut clicked = false;
    egui::Panel::left("log-files").exact_size(260.0).resizable(false).frame(egui::Frame::new().inner_margin(egui::Margin { right: 12, ..Default::default() })).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("log-files").auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for f in &files {
                let on = f.path == sel;
                let sub = format!("{}  ·  {}", modified_label(f.modified), w::size_label(f.size));
                let glyph = if f.crash { "󰅚" } else { "󰈙" };
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
        });
    });
    ui.add_space(4.0);
    match &s.loaded {
        Some((p, Ok(lines))) if *p == sel => {
            let shown = filtered(lines.iter(), &s.filter);
            if copy {
                ui.ctx().copy_text(shown.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"));
            }
            render_lines(ui, &t, &shown, false, view_nav, focus == Zone::Side);
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
}
