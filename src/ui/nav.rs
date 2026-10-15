//! Keyboard navigation. The screen is split into focus zones (the instance
//! list, the current tab's main pane, and a side pane where there is one);
//! the same keys move and act in whichever zone has focus. Keys come from
//! the user's [`Keymap`]; `App::keys` turns them into a [`Nav`], and each
//! pane takes it with `App::nav_for(zone)` while drawing, since that's where
//! its items live.

use egui::{RichText, Ui};

use super::instances::InstTab;
use crate::keymap::{self, Action, Keymap};
use super::widgets as w;
use super::{App, Tab};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    List,
    Main,
    Side,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Top,
    Bottom,
    PageUp,
    PageDown,
    Activate,
    Toggle,
    Delete,
    Rename,
    Duplicate,
    Edit,
    Add,
    Copy,
    Folder,
    MoveUp,
    MoveDown,
    Search,
    /// Dialogs only (on the main screen h/l move focus between panes).
    Left,
    Right,
    /// Cycle a setting, e.g. a group's colour.
    Cycle,
}

impl Nav {
    /// Apply a movement to a selection in a list of `len` items.
    pub fn step(self, len: usize, cur: Option<usize>) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let last = len as isize - 1;
        let page = 10;
        let at = cur.map(|c| c as isize);
        let to = match self {
            Nav::Down => at.map_or(0, |c| c + 1),
            Nav::Up => at.map_or(0, |c| c - 1),
            Nav::PageDown => at.map_or(0, |c| c + page),
            Nav::PageUp => at.map_or(0, |c| c - page),
            Nav::Top => 0,
            Nav::Bottom => last,
            _ => return cur,
        };
        Some(to.clamp(0, last) as usize)
    }

    pub fn is_move(self) -> bool {
        matches!(self, Nav::Up | Nav::Down | Nav::Top | Nav::Bottom | Nav::PageUp | Nav::PageDown)
    }

    /// Pixels to scroll a free-scrolling pane (logs) by.
    pub fn scroll_delta(self, line: f32, page: f32) -> Option<f32> {
        Some(match self {
            Nav::Down => -3.0 * line,
            Nav::Up => 3.0 * line,
            Nav::PageDown => -page,
            Nav::PageUp => page,
            Nav::Top => 1e9,
            Nav::Bottom => -1e9,
            _ => return None,
        })
    }
}

/// Which zones the current instance tab has.
pub fn has_side(app: &App) -> bool {
    app.inst_tab.content_kind().is_some() || app.inst_tab == InstTab::LogHistory || (app.inst_tab == InstTab::Version && app.version.picker_open())
}

/// Actions that drive panes and dialogs.
pub const PANE_ACTIONS: &[Action] = &[
    Action::Down,
    Action::Up,
    Action::Top,
    Action::Bottom,
    Action::PageDown,
    Action::PageUp,
    Action::Activate,
    Action::Toggle,
    Action::Delete,
    Action::Rename,
    Action::Duplicate,
    Action::Edit,
    Action::Add,
    Action::Copy,
    Action::Folder,
    Action::MoveDown,
    Action::MoveUp,
    Action::Search,
    Action::Cycle,
];

/// The pane-level meaning of an action. In dialogs the focus keys move
/// left/right instead.
pub fn to_nav(a: Action, modal: bool) -> Option<Nav> {
    Some(match a {
        Action::Down => Nav::Down,
        Action::Up => Nav::Up,
        Action::Top => Nav::Top,
        Action::Bottom => Nav::Bottom,
        Action::PageDown => Nav::PageDown,
        Action::PageUp => Nav::PageUp,
        Action::Activate => Nav::Activate,
        Action::Toggle => Nav::Toggle,
        Action::Delete => Nav::Delete,
        Action::Rename => Nav::Rename,
        Action::Duplicate => Nav::Duplicate,
        Action::Edit => Nav::Edit,
        Action::Add => Nav::Add,
        Action::Copy => Nav::Copy,
        Action::Folder => Nav::Folder,
        Action::MoveDown => Nav::MoveDown,
        Action::MoveUp => Nav::MoveUp,
        Action::Search => Nav::Search,
        Action::Cycle => Nav::Cycle,
        Action::FocusLeft if modal => Nav::Left,
        Action::FocusRight if modal => Nav::Right,
        _ => return None,
    })
}

/// Hints for the bottom bar: what the focused zone can do right now.
pub fn hints(app: &App) -> Vec<(String, &'static str)> {
    use Action::*;
    let k: &Keymap = &app.keymap;
    let l = |a| k.label(a);
    let p = |a, b| k.pair(a, b);
    if app.capture.is_some() {
        return vec![("any key".into(), "bind it"), ("esc".into(), "cancel")];
    }
    if app.modal.is_some() {
        return vec![(l(Help), "keys"), ("esc".into(), "close"), ("󰌑".into(), "confirm"), (p(Down, Up), "move")];
    }
    // `?` leads, so the way to every other key is always in the same place.
    let mut h: Vec<(String, &str)> = vec![(l(Help), "keys")];
    match app.tab {
        Tab::Accounts if app.acct.has_pending() => {
            h.extend([(l(Activate), "upload skin"), (l(Back), "cancel"), (l(Toggle), "classic/slim"), (p(TurnLeft, TurnRight), "turn")])
        }
        Tab::Accounts => {
            h.extend([(p(Down, Up), "accounts"), (l(Activate), "make active"), (l(Delete), "remove"), (p(TurnLeft, TurnRight), "turn")]);
            if super::accounts::current_is_microsoft(app) {
                h.extend([(l(UploadSkin), "skin"), (l(Toggle), "classic/slim"), (l(Cycle), "cape")]);
            }
            h.extend([(l(Microsoft), "microsoft"), (l(Add), "offline")]);
        }
        Tab::Settings => h.extend([("tab".into(), "next field")]),
        Tab::Instances if app.focus == Zone::List => h.extend([
            (l(Activate), "play"),
            (p(Down, Up), "instances"),
            (l(FocusRight), "into tab"),
            (p(PrevTab, NextTab), "tabs"),
            (l(Groups), "groups"),
            (l(New), "new"),
            (l(Import), "import"),
            (l(Export), "export"),
            (l(Edit), "edit"),
            (l(Cycle), "icon"),
            (l(Search), "filter"),
        ]),
        Tab::Instances => {
            let side = app.focus == Zone::Side;
            let lt = l(LoaderType);
            h.extend(match (app.inst_tab, side) {
                (InstTab::Version, false) if app.version.picker_open() => vec![(p(Down, Up), "versions"), (l(FocusRight), "loader"), (lt, "loader type"), (l(Activate), "apply"), (l(Back), "cancel")],
                (InstTab::Version, true) => vec![(p(Down, Up), "loader versions"), (lt, "loader type"), (l(Activate), "apply"), (l(Back), "cancel")],
                (InstTab::Version, _) => vec![(l(Activate), "change version")],
                (InstTab::Log, _) => vec![(p(Down, Up), "scroll"), (l(Bottom), "follow"), (l(Search), "filter"), (l(Copy), "copy")],
                (InstTab::Notes, _) => vec![(l(Activate), "write"), ("esc".into(), "stop writing")],
                (InstTab::Worlds, _) => vec![(p(Down, Up), "worlds"), (l(Activate), "play world"), (l(Rename), "rename"), (l(Duplicate), "duplicate"), (l(Delete), "delete"), (l(Folder), "folder")],
                (InstTab::Servers, _) => vec![(p(Down, Up), "servers"), (l(Activate), "join"), (l(Add), "add"), (l(Edit), "edit"), (p(MoveDown, MoveUp), "reorder"), (l(Delete), "remove")],
                (InstTab::Screenshots, _) => vec![(p(Down, Up), "screenshots"), (l(Activate), "open"), (l(Copy), "copy"), (l(Delete), "delete"), (l(Folder), "folder")],
                (InstTab::LogHistory, false) => vec![(p(Down, Up), "files"), (l(FocusRight), "read"), (l(Activate), "open"), (l(Folder), "folder")],
                (InstTab::LogHistory, true) => vec![(p(Down, Up), "scroll"), (l(Search), "filter"), (l(Copy), "copy")],
                (_, false) => vec![(p(Down, Up), "installed"), (l(Toggle), "enable/disable"), (l(Delete), "remove"), (l(FocusRight), "modrinth"), (l(Search), "search")],
                (_, true) => vec![(p(Down, Up), "results"), (l(Activate), "install"), (l(Search), "search")],
            });
            h.extend([(l(FocusLeft), "back"), (p(PrevTab, NextTab), "tabs")]);
        }
    }
    h.push((l(Play), "play"));
    h
}

/// The full reference, shown with `?`.
pub fn help(app: &App, ui: &mut Ui) -> bool {
    let t = &app.t;
    let screen = ui.ctx().content_rect();
    ui.set_width((screen.width() - 120.0).clamp(420.0, 860.0));
    ui.horizontal(|ui| {
        w::heading(ui, t, "keys");
        ui.add_space(12.0);
        ui.label(RichText::new("change them in settings → keys").color(t.muted));
    });
    ui.add_space(8.0);
    let mut sections: Vec<(&str, Vec<(String, &str)>)> = Vec::new();
    for i in keymap::ACTIONS {
        let keys = app.keymap.chords(i.action).iter().map(|c| c.label()).collect::<Vec<_>>().join(" / ");
        match sections.iter_mut().find(|(s, _)| *s == i.section) {
            Some((_, v)) => v.push((keys, i.what)),
            None => sections.push((i.section, vec![(keys, i.what)])),
        }
    }
    sections.push(("dialogs", vec![("tab".into(), "next field"), ("↑ / ↓".into(), "pick a version while typing a filter"), ("󰌑".into(), "confirm"), ("esc".into(), "close")]));
    let half = sections.iter().map(|(_, v)| v.len()).sum::<usize>() / 2;
    egui::ScrollArea::vertical().max_height(screen.height() - 220.0).show(ui, |ui| {
        ui.columns(2, |cols| {
            let mut count = 0;
            for (title, keys) in &sections {
                let ui = &mut cols[if count < half { 0 } else { 1 }];
                count += keys.len();
                w::section(ui, t, title);
                for (k, what) in keys {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(t.body_size() * 9.0, t.body_size() + 6.0), egui::Sense::hover());
                        ui.painter().with_clip_rect(rect).text(rect.left_center(), egui::Align2::LEFT_CENTER, k, egui::FontId::monospace(t.body_size()), t.accent);
                        ui.add(egui::Label::new(RichText::new(*what).color(t.fg)).truncate());
                    });
                }
                ui.add_space(8.0);
            }
        });
    });
    ui.add_space(8.0);
    w::button(ui, "close").clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The modal key route takes ⏎ off egui's event list, so dialogs must get it as `Activate`.
    #[test]
    fn enter_reaches_dialogs() {
        let km = Keymap::new(&Default::default());
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() });
        ctx.begin_pass(input);
        let nav = km.take(&ctx, PANE_ACTIONS).and_then(|a| to_nav(a, true));
        assert_eq!(nav, Some(Nav::Activate));
        ctx.end_pass().textures_delta.clear();
    }
}
