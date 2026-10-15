//! Omarchy theme integration.
//!
//! The active theme lives in `~/.local/state/omarchy/current/theme`, which
//! `omarchy theme set` replaces wholesale with `mv`. We read the palette from
//! `colors.toml` and the control/menu tokens from the generated `shell.toml`
//! (the same tokens the Omarchy shell's menus and launcher are drawn with),
//! then poll for changes so the app re-skins live when the theme changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Shadow, Stroke};

use crate::paths;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub dark: bool,

    pub accent: Color32,
    pub selection: Color32,
    pub muted: Color32,

    pub bg: Color32,
    pub bg_dark: Color32,
    pub bg_darker: Color32,
    pub bg_light: Color32,

    pub fg: Color32,
    pub fg_dark: Color32,
    pub fg_light: Color32,

    pub red: Color32,
    pub yellow: Color32,
    pub orange: Color32,
    pub green: Color32,
    pub cyan: Color32,
    pub blue: Color32,
    pub magenta: Color32,

    /// Hyprland active window border; Omarchy draws popups/menus with it.
    pub border_active: Color32,

    // [controls] from shell.toml
    pub normal_fill_alpha: f32,
    pub normal_border_alpha: f32,
    pub hover_fill_alpha: f32,
    pub hover_border_alpha: f32,
    pub selected_fill_alpha: f32,
    pub pressed_fill_alpha: f32,
    pub selection_fill_alpha: f32,

    // [menu] from shell.toml
    pub menu_selected_text: Color32,
    pub menu_selected_bg_alpha: f32,
    pub menu_selected_border_alpha: f32,
    pub scrim_alpha: f32,

    pub font_size: f32,
}

impl Default for Theme {
    /// Omarchy's stock Tokyo Night (colors.toml plus the shell.toml Omarchy
    /// generates from it), for running outside Omarchy. Kept identical by
    /// the `fallback_is_omarchy_tokyo_night` test.
    fn default() -> Self {
        let c = |s| parse_color(s).unwrap();
        Self {
            name: "tokyo-night".into(),
            dark: true,
            accent: c("#7aa2f7"),
            selection: c("#292e42"),
            muted: c("#414868"),
            bg: c("#1a1b26"),
            bg_dark: c("#13141c"),
            bg_darker: c("#0e0e14"),
            bg_light: c("#24283b"),
            fg: c("#a9b1d6"),
            fg_dark: c("#565f89"),
            fg_light: c("#b4bee6"),
            red: c("#f7768e"),
            yellow: c("#e0af68"),
            orange: c("#eb927b"),
            green: c("#9ece6a"),
            cyan: c("#449dab"),
            blue: c("#7aa2f7"),
            magenta: c("#ad8ee6"),
            border_active: c("#7aa2f7"),
            normal_fill_alpha: 0.04,
            normal_border_alpha: 0.4,
            hover_fill_alpha: 0.08,
            hover_border_alpha: 0.25,
            selected_fill_alpha: 0.18,
            pressed_fill_alpha: 0.22,
            selection_fill_alpha: 0.35,
            menu_selected_text: c("#7aa2f7"),
            menu_selected_bg_alpha: 0.08,
            menu_selected_border_alpha: 0.25,
            scrim_alpha: 0.5,
            font_size: 12.0,
        }
    }
}

pub fn parse_color(s: &str) -> Option<Color32> {
    let s = s.trim();
    // Gradients ("rgba(..) rgba(..) 45deg") collapse to their first stop.
    let s = s.split_whitespace().next()?;
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            6 => Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)),
            8 => Some(Color32::from_rgba_unmultiplied(
                (v >> 24) as u8,
                (v >> 16) as u8,
                (v >> 8) as u8,
                v as u8,
            )),
            _ => None,
        };
    }
    if let Some(inner) = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")) {
        let hex = inner.trim_end_matches(')');
        return parse_color(&format!("#{hex}"));
    }
    None
}

impl Theme {
    pub fn load() -> Self {
        Self::load_from(&paths::omarchy_theme_dir())
    }

    /// Read an Omarchy theme directory; anything missing keeps the fallback.
    pub fn load_from(dir: &std::path::Path) -> Self {
        let mut t = Theme::default();
        let Ok(colors) = std::fs::read_to_string(dir.join("colors.toml")) else {
            return t;
        };
        let Ok(colors) = colors.parse::<toml::Table>() else {
            return t;
        };
        let get = |k: &str| colors.get(k).and_then(|v| v.as_str()).and_then(parse_color);

        t.name = std::fs::read_to_string(dir.parent().unwrap_or(dir).join("theme.name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        t.dark = colors.get("mode").and_then(|v| v.as_str()) != Some("light");

        macro_rules! set {
            ($field:ident, $key:literal) => {
                if let Some(c) = get($key) {
                    t.$field = c;
                }
            };
        }
        set!(accent, "accent");
        set!(selection, "selection");
        set!(muted, "muted");
        set!(bg, "background");
        set!(fg, "foreground");
        set!(red, "red");
        set!(yellow, "yellow");
        set!(orange, "orange");
        set!(green, "green");
        set!(cyan, "cyan");
        set!(blue, "blue");
        set!(magenta, "magenta");
        // Optional keys: older themes omit these, so derive from bg/fg.
        t.bg_dark = get("dark_background").unwrap_or_else(|| shade(t.bg, t.dark, 0.25));
        t.bg_darker = get("darker_background").unwrap_or_else(|| shade(t.bg, t.dark, 0.45));
        t.bg_light = get("lighter_background").unwrap_or_else(|| t.bg.lerp_to_gamma(t.fg, 0.08));
        t.fg_dark = get("dark_foreground").unwrap_or(t.muted);
        t.fg_light = get("light_foreground").unwrap_or(t.fg);
        t.border_active = t.accent;
        t.menu_selected_text = t.accent;

        if let Ok(shell) = std::fs::read_to_string(dir.join("shell.toml"))
            && let Ok(shell) = shell.parse::<toml::Table>()
        {
            t.apply_shell_tokens(&shell);
        }
        t
    }

    fn apply_shell_tokens(&mut self, shell: &toml::Table) {
        let section = |name: &str| shell.get(name).and_then(|v| v.as_table());
        let f = |tbl: Option<&toml::Table>, k: &str| {
            tbl.and_then(|t| t.get(k)).and_then(|v| {
                v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
            })
        };
        let hypr = section("hyprland");
        if let Some(c) = hypr
            .and_then(|h| h.get("active-border"))
            .and_then(|v| v.as_str())
            .and_then(parse_color)
        {
            self.border_active = c;
        }

        let ctl = section("controls");
        macro_rules! alpha {
            ($tbl:expr, $field:ident, $key:literal) => {
                if let Some(v) = f($tbl, $key) {
                    self.$field = v as f32;
                }
            };
        }
        alpha!(ctl, normal_fill_alpha, "normal-fill-alpha");
        alpha!(ctl, normal_border_alpha, "normal-border-alpha");
        alpha!(ctl, hover_fill_alpha, "hover-cursor-fill-alpha");
        alpha!(ctl, hover_border_alpha, "hover-cursor-border-alpha");
        alpha!(ctl, selected_fill_alpha, "selected-fill-alpha");
        alpha!(ctl, pressed_fill_alpha, "pressed-fill-alpha");
        alpha!(ctl, selection_fill_alpha, "selection-fill-alpha");

        let menu = section("menu");
        alpha!(menu, menu_selected_bg_alpha, "selected-background-alpha");
        alpha!(menu, menu_selected_border_alpha, "selected-border-alpha");
        alpha!(menu, scrim_alpha, "scrim-alpha");
        if let Some(c) = menu
            .and_then(|m| m.get("selected-text"))
            .and_then(|v| v.as_str())
            .and_then(parse_color)
        {
            self.menu_selected_text = c;
        }

        if let Some(size) = f(section("font"), "base-size") {
            self.font_size = size as f32;
        }
    }

    pub fn alpha(c: Color32, a: f32) -> Color32 {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
    }

    pub fn fg_a(&self, a: f32) -> Color32 {
        Self::alpha(self.fg, a)
    }

    /// Body text size. Omarchy's base-size 12 is tuned for the bar; a full
    /// window reads better slightly larger, scaled with the user's setting.
    pub fn body_size(&self) -> f32 {
        (self.font_size * 1.1).round()
    }

    /// Palette colour by name, for things stored by name so they re-theme.
    pub fn named(&self, name: &str) -> Option<Color32> {
        Some(match name {
            "red" => self.red,
            "orange" => self.orange,
            "yellow" => self.yellow,
            "green" => self.green,
            "cyan" => self.cyan,
            "blue" => self.blue,
            "magenta" => self.magenta,
            _ => return None,
        })
    }

    pub fn apply(&self, ctx: &egui::Context) {
        ctx.set_visuals(if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() });
        let size = self.body_size();
        let square = CornerRadius::ZERO;
        ctx.all_styles_mut(|style| {
            use egui::{FontId, TextStyle};
            style.text_styles = [
                (TextStyle::Small, FontId::monospace(size - 2.0)),
                (TextStyle::Body, FontId::monospace(size)),
                (TextStyle::Button, FontId::monospace(size)),
                (TextStyle::Monospace, FontId::monospace(size)),
                (TextStyle::Heading, FontId::monospace(size * 1.6)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.spacing.interact_size.y = size + 12.0;
            style.spacing.scroll.floating = true;
            // Narrow windows clip text at the edge rather than reflowing it;
            // paragraphs opt back in to wrapping (see widgets::para).
            style.wrap_mode = Some(egui::TextWrapMode::Extend);

            let v = &mut style.visuals;
            v.dark_mode = self.dark;
            v.override_text_color = None;
            v.panel_fill = self.bg;
            v.window_fill = self.bg;
            v.window_stroke = Stroke::new(2.0, self.border_active);
            v.window_corner_radius = square;
            v.menu_corner_radius = square;
            v.window_shadow = Shadow::NONE;
            v.popup_shadow = Shadow::NONE;
            v.extreme_bg_color = self.bg_dark;
            v.text_edit_bg_color = Some(self.bg_dark);
            v.code_bg_color = self.bg_dark;
            v.faint_bg_color = self.bg_light;
            v.hyperlink_color = self.accent;
            v.warn_fg_color = self.yellow;
            v.error_fg_color = self.red;
            v.weak_text_color = Some(self.muted);
            v.selection.bg_fill = Self::alpha(self.accent, self.selection_fill_alpha);
            v.selection.stroke = Stroke::new(1.0, self.accent);
            v.text_cursor.stroke = Stroke::new(2.0, self.accent);
            v.striped = false;
            v.slider_trailing_fill = true;
            v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.5 };
            v.indent_has_left_vline = false;

            let w = &mut v.widgets;
            w.noninteractive.bg_fill = self.bg;
            w.noninteractive.weak_bg_fill = self.bg;
            w.noninteractive.bg_stroke = Stroke::new(1.0, self.fg_a(0.12));
            w.noninteractive.fg_stroke = Stroke::new(1.0, self.fg);

            let fill = |a| self.fg_a(a);
            w.inactive.bg_fill = self.fg_a(0.14);
            w.inactive.weak_bg_fill = fill(self.normal_fill_alpha);
            w.inactive.bg_stroke = Stroke::new(1.0, self.fg_a(self.normal_border_alpha));
            w.inactive.fg_stroke = Stroke::new(1.0, self.fg);

            w.hovered.bg_fill = fill(self.hover_fill_alpha);
            w.hovered.weak_bg_fill = fill(self.hover_fill_alpha);
            w.hovered.bg_stroke = Stroke::new(1.0, self.accent);
            w.hovered.fg_stroke = Stroke::new(1.0, self.fg_light);

            w.active.bg_fill = fill(self.pressed_fill_alpha);
            w.active.weak_bg_fill = fill(self.pressed_fill_alpha);
            w.active.bg_stroke = Stroke::new(1.0, self.accent);
            w.active.fg_stroke = Stroke::new(1.0, self.accent);

            w.open.bg_fill = fill(self.selected_fill_alpha);
            w.open.weak_bg_fill = fill(self.selected_fill_alpha);
            w.open.bg_stroke = Stroke::new(1.0, self.accent);
            w.open.fg_stroke = Stroke::new(1.0, self.accent);

            for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                wv.corner_radius = square;
                wv.expansion = 0.0;
            }
        });
    }
}

fn shade(c: Color32, dark: bool, amt: f32) -> Color32 {
    let target = if dark { Color32::BLACK } else { Color32::WHITE };
    c.lerp_to_gamma(target, amt)
}

// ---------------------------------------------------------------------------
// Fonts: follow fontconfig's `monospace`, which is what `omarchy font set`
// rewrites. A Nerd Font is chained in after it for icon glyphs.

#[derive(Debug, Clone, PartialEq)]
pub struct FontSet {
    pub regular: Option<PathBuf>,
    pub bold: Option<PathBuf>,
    pub icons: Option<PathBuf>,
}

fn fc_match(pattern: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // egui can't read TrueType collections or variable-only WOFF.
    let ok = p.extension().is_some_and(|e| {
        let e = e.to_string_lossy().to_lowercase();
        e == "ttf" || e == "otf"
    });
    ok.then_some(p)
}

impl FontSet {
    pub fn resolve() -> Self {
        Self {
            regular: fc_match("monospace"),
            bold: fc_match("monospace:bold"),
            // fontconfig always returns *something*; only take real Nerd Fonts.
            icons: ["Symbols Nerd Font Mono", "JetBrainsMono Nerd Font"]
                .iter()
                .filter_map(|f| fc_match(f))
                .find(|p| p.to_string_lossy().contains("Nerd")),
        }
    }

    pub fn apply(&self, ctx: &egui::Context) {
        let mut defs = FontDefinitions::default();
        let mut mono: Vec<String> = Vec::new();
        let mut load = |name: &str, path: &Option<PathBuf>| -> bool {
            let Some(bytes) = path.as_ref().and_then(|p| std::fs::read(p).ok()) else {
                return false;
            };
            defs.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            true
        };
        if load("omarchy-mono", &self.regular) {
            mono.push("omarchy-mono".into());
        }
        let bold = load("omarchy-mono-bold", &self.bold);
        if load("omarchy-icons", &self.icons) {
            mono.push("omarchy-icons".into());
        }
        // Bundled last, so icons render even without a Nerd Font installed;
        // the system's own Nerd Font still wins when there is one.
        defs.font_data.insert("bundled-icons".into(), Arc::new(FontData::from_static(BUNDLED_ICONS)));
        mono.push("bundled-icons".into());
        let fam = defs.families.entry(FontFamily::Monospace).or_default();
        for (i, name) in mono.iter().enumerate() {
            fam.insert(i, name.clone());
        }
        // Proportional text also renders in the mono face: Omarchy is mono-first.
        let prop = defs.families.entry(FontFamily::Proportional).or_default();
        for (i, name) in mono.iter().enumerate() {
            prop.insert(i, name.clone());
        }
        if bold {
            let mut fam = vec!["omarchy-mono-bold".to_string()];
            fam.extend(mono.iter().skip(1).cloned());
            fam.extend(defs.families[&FontFamily::Monospace].iter().cloned());
            defs.families.insert(FontFamily::Name("bold".into()), fam);
        } else {
            let fam = defs.families[&FontFamily::Monospace].clone();
            defs.families.insert(FontFamily::Name("bold".into()), fam);
        }
        ctx.set_fonts(defs);
    }
}

/// Symbols Nerd Font Mono v3.5.1 (MIT), see assets/fonts.
static BUNDLED_ICONS: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

pub fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, FontFamily::Name("bold".into()))
}

// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Pending {
    pub theme: Option<Theme>,
    pub fonts: Option<FontSet>,
}

/// Polls the theme directory and fontconfig for changes. Polling (rather than
/// inotify) sidesteps the theme dir being replaced by `mv` under a watch.
pub fn spawn_watcher(ctx: egui::Context) -> Arc<Mutex<Pending>> {
    let pending = Arc::new(Mutex::new(Pending::default()));
    let out = pending.clone();
    std::thread::spawn(move || {
        let theme_dir = paths::omarchy_theme_dir();
        let fonts_conf = dirs::config_dir().unwrap_or_default().join("fontconfig/fonts.conf");
        let snapshot = || -> BTreeMap<&'static str, Vec<u8>> {
            let mut m = BTreeMap::new();
            for f in ["colors.toml", "shell.toml"] {
                m.insert(f, std::fs::read(theme_dir.join(f)).unwrap_or_default());
            }
            m
        };
        let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut last = snapshot();
        let mut last_font: Option<SystemTime> = mtime(&fonts_conf);
        loop {
            std::thread::sleep(Duration::from_millis(700));
            let now = snapshot();
            if now != last {
                last = now;
                // Give theme-set a moment to finish writing its files.
                std::thread::sleep(Duration::from_millis(150));
                out.lock().unwrap().theme = Some(Theme::load());
                ctx.request_repaint();
            }
            let font = mtime(&fonts_conf);
            if font != last_font {
                last_font = font;
                out.lock().unwrap().fonts = Some(FontSet::resolve());
                ctx.request_repaint();
            }
        }
    });
    pending
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off Omarchy we fall back to Tokyo Night; it must match Omarchy's own.
    #[test]
    fn fallback_is_omarchy_tokyo_night() {
        let dir = std::path::Path::new("/usr/share/omarchy/themes/tokyo-night");
        if !dir.join("colors.toml").exists() {
            return;
        }
        let mut stock = Theme::load_from(dir);
        stock.name = "tokyo-night".into();
        assert_eq!(stock, Theme::default());
    }
}
