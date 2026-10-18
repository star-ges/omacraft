//! Rebindable keys. Every action has default chords; the user's changes
//! live in `[keys]` in config.toml as `action = ["chord", …]`, e.g.
//!
//!   [keys]
//!   down = ["j", "down"]
//!   page_down = ["ctrl+d", "pagedown"]
//!   move_down = ["shift+j"]
//!
//! Chords are `[ctrl+][alt+][shift+]key`, where key is a letter, digit,
//! symbol (`/`, `[`, `?`…) or a name (enter, esc, space, tab, up, down,
//! left, right, home, end, pageup, pagedown, delete, f1…f12).

use std::collections::BTreeMap;

use egui::{Key, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    TabInstances,
    TabAccounts,
    TabSettings,
    Play,
    New,
    Import,
    Export,
    Groups,
    Help,
    Quit,
    FocusLeft,
    FocusRight,
    PrevTab,
    NextTab,
    Down,
    Up,
    Top,
    Bottom,
    PageDown,
    PageUp,
    Back,
    Activate,
    Toggle,
    Delete,
    Rename,
    Duplicate,
    Edit,
    Add,
    Copy,
    Folder,
    MoveDown,
    MoveUp,
    Search,
    Cycle,
    ModsTab,
    LoaderType,
    Update,
    UpdateOne,
    RollBack,
    Why,
    ModSets,
    FindBreaking,
    Microsoft,
    TurnLeft,
    TurnRight,
    UploadSkin,
}

/// Where an action applies; two actions may share a chord only if their
/// scopes never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    /// Panes and dialogs everywhere.
    Nav,
    Instances,
    Accounts,
}

pub struct Info {
    pub action: Action,
    pub id: &'static str,
    pub what: &'static str,
    pub section: &'static str,
    pub scope: Scope,
    pub defaults: &'static [&'static str],
}

macro_rules! actions {
    ($(($a:ident, $id:literal, $what:literal, $section:literal, $scope:ident, [$($d:literal),*])),* $(,)?) => {
        pub const ACTIONS: &[Info] = &[$(Info {
            action: Action::$a,
            id: $id,
            what: $what,
            section: $section,
            scope: Scope::$scope,
            defaults: &[$($d),*],
        }),*];
    };
}

actions!(
    (TabInstances, "tab_instances", "instances tab", "everywhere", Global, ["1"]),
    (TabAccounts, "tab_accounts", "accounts tab", "everywhere", Global, ["2"]),
    (TabSettings, "tab_settings", "settings tab", "everywhere", Global, ["3"]),
    (Play, "play", "play the selected instance", "everywhere", Global, ["p"]),
    (New, "new", "new instance", "everywhere", Global, ["n"]),
    (Import, "import", "import an instance or pack", "everywhere", Global, ["i"]),
    (Export, "export", "export the selected instance", "everywhere", Global, ["shift+e"]),
    (Groups, "groups", "manage groups", "everywhere", Global, ["g"]),
    (Help, "help", "show all keys", "everywhere", Global, ["?"]),
    (Quit, "quit", "quit", "everywhere", Global, ["ctrl+q"]),
    (FocusLeft, "focus_left", "focus the pane to the left", "moving around", Nav, ["h", "left"]),
    (FocusRight, "focus_right", "focus the pane to the right", "moving around", Nav, ["l", "right"]),
    (PrevTab, "prev_tab", "previous instance tab", "moving around", Nav, ["["]),
    (NextTab, "next_tab", "next instance tab", "moving around", Nav, ["]"]),
    (Down, "down", "move down", "moving around", Nav, ["j", "down"]),
    (Up, "up", "move up", "moving around", Nav, ["k", "up"]),
    (Top, "top", "first item", "moving around", Nav, ["home"]),
    (Bottom, "bottom", "last item", "moving around", Nav, ["end"]),
    (PageDown, "page_down", "page down", "moving around", Nav, ["ctrl+d", "pagedown"]),
    (PageUp, "page_up", "page up", "moving around", Nav, ["ctrl+u", "pageup"]),
    (Back, "back", "back towards the instance list", "moving around", Nav, ["esc"]),
    (Search, "search", "search or filter the focused pane", "moving around", Nav, ["/"]),
    (Activate, "activate", "play, play world, join, install, open…", "acting on the selection", Nav, ["enter"]),
    (Toggle, "toggle", "enable / disable, add to group, all instances' screenshots", "acting on the selection", Nav, ["space"]),
    (Delete, "delete", "delete or remove", "acting on the selection", Nav, ["x", "delete"]),
    (Rename, "rename", "rename", "acting on the selection", Nav, ["r"]),
    (Duplicate, "duplicate", "duplicate", "acting on the selection", Nav, ["d"]),
    (Edit, "edit", "edit", "acting on the selection", Nav, ["e"]),
    (Add, "add", "add", "acting on the selection", Nav, ["a"]),
    (Copy, "copy", "copy", "acting on the selection", Nav, ["y"]),
    (Folder, "folder", "open folder", "acting on the selection", Nav, ["o"]),
    (MoveDown, "move_down", "move item down", "acting on the selection", Nav, ["shift+j"]),
    (MoveUp, "move_up", "move item up", "acting on the selection", Nav, ["shift+k"]),
    (Cycle, "cycle", "change icon / cycle group colour", "acting on the selection", Nav, ["c"]),
    (ModsTab, "mods_tab", "jump to the mods tab", "instances", Instances, ["m"]),
    (LoaderType, "loader_type", "cycle loader type (version tab)", "instances", Instances, ["t"]),
    (Update, "update", "check for mod updates, then update them all (mods tab)", "instances", Instances, ["u"]),
    (UpdateOne, "update_one", "update the selected mod (mods tab)", "instances", Instances, ["shift+u"]),
    (RollBack, "roll_back", "roll mods back to a backup (mods tab)", "instances", Instances, ["z"]),
    (Why, "why", "why the game crashed (log tabs)", "instances", Instances, ["w"]),
    (ModSets, "mod_sets", "mod sets: save, switch, rename (mods tab)", "instances", Instances, ["s"]),
    (FindBreaking, "find_breaking", "find the breaking mod, or answer its step", "instances", Instances, ["b"]),
    (Microsoft, "microsoft", "sign in with Microsoft", "accounts", Accounts, ["m"]),
    (TurnLeft, "turn_left", "turn the skin preview left", "accounts", Accounts, [","]),
    (TurnRight, "turn_right", "turn the skin preview right", "accounts", Accounts, ["."]),
    (UploadSkin, "upload_skin", "choose a skin to upload", "accounts", Accounts, ["u"]),
);

pub fn info(a: Action) -> &'static Info {
    ACTIONS.iter().find(|i| i.action == a).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// Symbols already imply shift on most layouts, so it's ignored for them.
fn shift_matters(k: Key) -> bool {
    if matches!(k, Key::ArrowDown | Key::ArrowUp | Key::ArrowLeft | Key::ArrowRight) {
        return true;
    }
    let sym = k.symbol_or_name();
    let mut chars = sym.chars();
    !matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_alphanumeric())
}

impl Chord {
    pub fn parse(s: &str) -> Option<Chord> {
        let s = s.trim();
        let mut c = Chord { key: Key::A, ctrl: false, alt: false, shift: false };
        let mut rest = s;
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(r) = lower.strip_prefix("ctrl+") {
                c.ctrl = true;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("alt+") {
                c.alt = true;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("shift+") {
                c.shift = true;
                rest = &rest[rest.len() - r.len()..];
            } else {
                break;
            }
        }
        // A lone capital letter means shift+letter.
        if rest.len() == 1 && rest.chars().all(|ch| ch.is_ascii_uppercase()) {
            c.shift = true;
        }
        let alias = match rest.to_ascii_lowercase().as_str() {
            "pgup" => "PageUp",
            "pgdn" | "pgdown" => "PageDown",
            "pageup" => "PageUp",
            "pagedown" => "PageDown",
            "del" => "Delete",
            "ins" => "Insert",
            "return" | "󰌑" => "Enter",
            "↑" => "Up",
            "↓" => "Down",
            "←" => "Left",
            "→" => "Right",
            _ => "",
        };
        let name = if alias.is_empty() { rest.to_string() } else { alias.to_string() };
        let capitalized = {
            let mut ch = name.chars();
            ch.next().map(|f| f.to_uppercase().chain(ch.flat_map(|c| c.to_lowercase())).collect::<String>()).unwrap_or_default()
        };
        c.key = Key::from_name(&name).or_else(|| Key::from_name(&capitalized)).or_else(|| Key::from_name(&name.to_uppercase()))?;
        Some(c)
    }

    pub fn from_event(key: Key, m: Modifiers) -> Chord {
        Chord { key, ctrl: m.ctrl || m.command, alt: m.alt, shift: m.shift && shift_matters(key) }
    }

    fn matches(&self, key: Key, m: Modifiers) -> bool {
        self.key == key && self.ctrl == (m.ctrl || m.command) && self.alt == m.alt && (!shift_matters(key) || self.shift == m.shift)
    }

    /// For config files: `ctrl+d`, `shift+j`, `/`.
    pub fn spec(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl+");
        }
        if self.alt {
            s.push_str("alt+");
        }
        if self.shift {
            s.push_str("shift+");
        }
        let name = self.key.name();
        if name.len() == 1 {
            s.push_str(&name.to_lowercase());
        } else {
            s.push_str(&match self.key {
                Key::Slash | Key::Questionmark | Key::OpenBracket | Key::CloseBracket | Key::Minus | Key::Equals | Key::Comma | Key::Period => {
                    self.key.symbol_or_name().to_string()
                }
                _ => name.to_lowercase(),
            });
        }
        s
    }

    /// For people: `J`, `ctrl+d`, `⏎`, `↓`.
    pub fn label(&self) -> String {
        let key = match self.key {
            Key::Enter => "󰌑".to_string(),
            Key::ArrowDown => "↓".into(),
            Key::ArrowUp => "↑".into(),
            Key::ArrowLeft => "←".into(),
            Key::ArrowRight => "→".into(),
            Key::Escape => "esc".into(),
            Key::PageDown => "pgdn".into(),
            Key::PageUp => "pgup".into(),
            Key::Delete => "del".into(),
            k if k.name().len() == 1 => {
                let n = k.name();
                if self.shift && n.chars().all(|c| c.is_ascii_alphabetic()) {
                    return format!("{}{}{n}", if self.ctrl { "ctrl+" } else { "" }, if self.alt { "alt+" } else { "" });
                }
                n.to_lowercase()
            }
            k => k.symbol_or_name().to_lowercase(),
        };
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl+");
        }
        if self.alt {
            s.push_str("alt+");
        }
        if self.shift {
            s.push_str("shift+");
        }
        s + &key
    }
}

#[derive(Debug, Clone)]
pub struct Keymap {
    map: BTreeMap<Action, Vec<Chord>>,
}

impl Keymap {
    /// Defaults, with the user's `[keys]` entries on top.
    pub fn new(overrides: &BTreeMap<String, Vec<String>>) -> Self {
        let mut map = BTreeMap::new();
        for i in ACTIONS {
            let specs: Vec<String> = overrides.get(i.id).cloned().unwrap_or_else(|| i.defaults.iter().map(|s| s.to_string()).collect());
            map.insert(i.action, specs.iter().filter_map(|s| Chord::parse(s)).collect());
        }
        Keymap { map }
    }

    pub fn chords(&self, a: Action) -> &[Chord] {
        self.map.get(&a).map(Vec::as_slice).unwrap_or(&[])
    }

    /// First chord's label, for hints.
    pub fn label(&self, a: Action) -> String {
        self.chords(a).first().map(Chord::label).unwrap_or_else(|| "—".into())
    }

    /// `j/k` style label for a pair of actions.
    pub fn pair(&self, a: Action, b: Action) -> String {
        format!("{}/{}", self.label(a), self.label(b))
    }

    /// Find and consume this frame's key press for the first matching
    /// action among `candidates`.
    pub fn take(&self, ctx: &egui::Context, candidates: &[Action]) -> Option<Action> {
        ctx.input_mut(|i| {
            for (n, ev) in i.events.iter().enumerate() {
                let egui::Event::Key { key, pressed: true, modifiers, .. } = ev else { continue };
                // Prefer chords with more modifiers (shift+j over j).
                let mut best: Option<(Action, u8)> = None;
                for &a in candidates {
                    for c in self.chords(a) {
                        if c.matches(*key, *modifiers) {
                            let weight = c.ctrl as u8 + c.alt as u8 + c.shift as u8;
                            if best.is_none_or(|(_, w)| weight > w) {
                                best = Some((a, weight));
                            }
                        }
                    }
                }
                if let Some((a, _)) = best {
                    i.events.remove(n);
                    return Some(a);
                }
            }
            None
        })
    }

    /// Other actions this chord is also bound to, where scopes overlap.
    pub fn conflicts(&self, a: Action, c: &Chord) -> Vec<Action> {
        let scope = info(a).scope;
        self.map
            .iter()
            .filter(|(b, chords)| **b != a && chords.contains(c))
            .filter(|(b, _)| {
                let other = info(**b).scope;
                !matches!((scope, other), (Scope::Instances, Scope::Accounts) | (Scope::Accounts, Scope::Instances))
            })
            .map(|(b, _)| *b)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_round_trip() {
        for s in ["j", "shift+j", "ctrl+d", "/", "?", "[", ",", ".", "enter", "esc", "pagedown", "f5", "ctrl+alt+x"] {
            let c = Chord::parse(s).unwrap_or_else(|| panic!("parse {s}"));
            assert_eq!(Chord::parse(&c.spec()), Some(c), "{s} → {}", c.spec());
        }
        assert_eq!(Chord::parse("J"), Chord::parse("shift+j"));
        assert_eq!(Chord::parse("J").unwrap().label(), "J");
        assert!(Chord::parse("nonsense").is_none());
    }

    #[test]
    fn defaults_have_no_conflicts() {
        let km = Keymap::new(&BTreeMap::new());
        for i in ACTIONS {
            for c in km.chords(i.action) {
                assert!(km.conflicts(i.action, c).is_empty(), "{} {:?}: {:?}", i.id, c, km.conflicts(i.action, c));
            }
        }
    }

    #[test]
    fn overrides_apply() {
        let mut o = BTreeMap::new();
        o.insert("down".to_string(), vec!["n".to_string()]);
        let km = Keymap::new(&o);
        assert_eq!(km.label(Action::Down), "n");
        assert_eq!(km.conflicts(Action::Down, &Chord::parse("n").unwrap()), vec![Action::New]);
    }
}
