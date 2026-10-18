//! Minecraft's own settings, `options.txt` in the game folder: one `key:value` per line.
//! Read into the config editor's fields, grouped and named like the game's menus, with
//! ranges and choices for the settings people actually change. Keys it doesn't know
//! still show (under "other"), so nothing is hidden. Edits replace only the value's bytes,
//! like every config.

use crate::mod_config::{Doc, Field, Hint, Scalar, Value};

/// Where a setting goes, what it's called, and what it takes.
struct Known {
    key: &'static str,
    section: &'static str,
    label: &'static str,
    doc: &'static str,
    kind: Kind,
    /// The game's default, as written in the file (without quotes).
    default: &'static str,
}

enum Kind {
    Plain,
    /// A number between these.
    Range(f64, f64),
    /// One of these values (as written in the file), with the game's name for each.
    Choice(&'static [(&'static str, &'static str)]),
}

use Kind::*;

const fn k(key: &'static str, section: &'static str, label: &'static str, doc: &'static str, kind: Kind, default: &'static str) -> Known {
    Known { key, section, label, doc, kind, default }
}

/// The game's own key bindings and their defaults (mods add more, whose defaults aren't known here).
const BINDINGS: &[(&str, &str, &str)] = &[
    ("key_key.attack", "attack / destroy", "key.mouse.left"),
    ("key_key.use", "use item / place block", "key.mouse.right"),
    ("key_key.forward", "walk forwards", "key.keyboard.w"),
    ("key_key.left", "strafe left", "key.keyboard.a"),
    ("key_key.back", "walk backwards", "key.keyboard.s"),
    ("key_key.right", "strafe right", "key.keyboard.d"),
    ("key_key.jump", "jump", "key.keyboard.space"),
    ("key_key.sneak", "sneak", "key.keyboard.left.shift"),
    ("key_key.sprint", "sprint", "key.keyboard.left.control"),
    ("key_key.drop", "drop selected item", "key.keyboard.q"),
    ("key_key.inventory", "open/close inventory", "key.keyboard.e"),
    ("key_key.swapOffhand", "swap item with offhand", "key.keyboard.f"),
    ("key_key.pickItem", "pick block", "key.mouse.middle"),
    ("key_key.chat", "open chat", "key.keyboard.t"),
    ("key_key.command", "open command", "key.keyboard.slash"),
    ("key_key.playerlist", "list players", "key.keyboard.tab"),
    ("key_key.socialInteractions", "social interactions", "key.keyboard.p"),
    ("key_key.advancements", "advancements", "key.keyboard.l"),
    ("key_key.screenshot", "take screenshot", "key.keyboard.f2"),
    ("key_key.togglePerspective", "toggle perspective", "key.keyboard.f5"),
    ("key_key.smoothCamera", "cinematic camera", "key.keyboard.unknown"),
    ("key_key.fullscreen", "toggle fullscreen", "key.keyboard.f11"),
    ("key_key.spectatorOutlines", "highlight players (spectators)", "key.keyboard.unknown"),
    ("key_key.saveToolbarActivator", "save hotbar activator", "key.keyboard.c"),
    ("key_key.loadToolbarActivator", "load hotbar activator", "key.keyboard.x"),
    ("key_key.hotbar.1", "hotbar slot 1", "key.keyboard.1"),
    ("key_key.hotbar.2", "hotbar slot 2", "key.keyboard.2"),
    ("key_key.hotbar.3", "hotbar slot 3", "key.keyboard.3"),
    ("key_key.hotbar.4", "hotbar slot 4", "key.keyboard.4"),
    ("key_key.hotbar.5", "hotbar slot 5", "key.keyboard.5"),
    ("key_key.hotbar.6", "hotbar slot 6", "key.keyboard.6"),
    ("key_key.hotbar.7", "hotbar slot 7", "key.keyboard.7"),
    ("key_key.hotbar.8", "hotbar slot 8", "key.keyboard.8"),
    ("key_key.hotbar.9", "hotbar slot 9", "key.keyboard.9"),
];

const VIDEO: &str = "video";
const SOUND: &str = "sound";
const CONTROLS: &str = "controls";
const CHAT: &str = "chat";
const ACCESS: &str = "accessibility";
const SKIN: &str = "skin";
const GAME: &str = "game";

/// The settings worth a name, in menu order within each section.
const KNOWN: &[Known] = &[
    k("renderDistance", VIDEO, "render distance", "How many chunks away the world is drawn.", Range(2.0, 32.0), "12"),
    k("simulationDistance", VIDEO, "simulation distance", "How many chunks away things keep moving and growing (in single player).", Range(5.0, 32.0), "12"),
    k("graphicsMode", VIDEO, "graphics", "", Choice(&[("0", "fast"), ("1", "fancy"), ("2", "fabulous")]), "1"),
    k("maxFps", VIDEO, "max framerate", "260 means unlimited.", Range(10.0, 260.0), "120"),
    k("enableVsync", VIDEO, "vsync", "", Plain, "true"),
    k("fullscreen", VIDEO, "fullscreen", "", Plain, "false"),
    k("guiScale", VIDEO, "gui scale", "0 is auto.", Range(0.0, 6.0), "0"),
    k("gamma", VIDEO, "brightness", "0 is moody, 1 is bright.", Range(0.0, 1.0), "0.5"),
    k("fov", VIDEO, "field of view", "Stored from -1 to 1: 0 is 70°, 1 is 110° (\"quake pro\"), -1 is 30°.", Range(-1.0, 1.0), "0.0"),
    k("renderClouds", VIDEO, "clouds", "", Choice(&[("true", "fancy"), ("fast", "fast"), ("false", "off")]), "true"),
    k("particles", VIDEO, "particles", "", Choice(&[("0", "all"), ("1", "decreased"), ("2", "minimal")]), "0"),
    k("ao", VIDEO, "smooth lighting", "", Plain, "true"),
    k("biomeBlendRadius", VIDEO, "biome blend", "How far grass, water and foliage colours blend between biomes; higher is smoother and slower.", Range(0.0, 7.0), "2"),
    k("entityDistanceScaling", VIDEO, "entity distance", "How far away entities are drawn, as a share of the usual distance.", Range(0.5, 5.0), "1.0"),
    k("entityShadows", VIDEO, "entity shadows", "", Plain, "true"),
    k("mipmapLevels", VIDEO, "mipmap levels", "Smooths textures far away.", Range(0.0, 4.0), "4"),
    k("prioritizeChunkUpdates", VIDEO, "chunk builder", "", Choice(&[("0", "threaded"), ("1", "semi blocking"), ("2", "fully blocking")]), "0"),
    k("bobView", VIDEO, "view bobbing", "", Plain, "true"),
    k("attackIndicator", VIDEO, "attack indicator", "", Choice(&[("0", "off"), ("1", "crosshair"), ("2", "hotbar")]), "1"),
    k("showAutosaveIndicator", VIDEO, "autosave indicator", "", Plain, "true"),
    k("menuBackgroundBlurriness", VIDEO, "menu background blur", "", Range(0.0, 10.0), "5"),
    k("glintSpeed", VIDEO, "glint speed", "How fast enchanted items shimmer.", Range(0.0, 1.0), "0.5"),
    k("glintStrength", VIDEO, "glint strength", "How strongly enchanted items shimmer.", Range(0.0, 1.0), "0.75"),
    k("soundCategory_master", SOUND, "master volume", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_music", SOUND, "music", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_record", SOUND, "jukebox / note blocks", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_weather", SOUND, "weather", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_block", SOUND, "blocks", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_hostile", SOUND, "hostile creatures", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_neutral", SOUND, "friendly creatures", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_player", SOUND, "players", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_ambient", SOUND, "ambient / environment", "", Range(0.0, 1.0), "1.0"),
    k("soundCategory_voice", SOUND, "voice / speech", "", Range(0.0, 1.0), "1.0"),
    k("showSubtitles", SOUND, "subtitles", "", Plain, "false"),
    k("directionalAudio", SOUND, "directional audio", "Simulates 3D sound; needs headphones.", Plain, "false"),
    k("soundDevice", SOUND, "device", "Empty means the system default.", Plain, ""),
    k("mouseSensitivity", CONTROLS, "mouse sensitivity", "0.5 is 100%.", Range(0.0, 1.0), "0.5"),
    k("invertYMouse", CONTROLS, "invert mouse", "", Plain, "false"),
    k("mouseWheelSensitivity", CONTROLS, "scroll sensitivity", "", Range(0.01, 10.0), "1.0"),
    k("discrete_mouse_scroll", CONTROLS, "discrete scrolling", "", Plain, "false"),
    k("rawMouseInput", CONTROLS, "raw input", "", Plain, "true"),
    k("autoJump", CONTROLS, "auto-jump", "", Plain, "false"),
    k("toggleCrouch", CONTROLS, "sneak", "On toggles, off holds.", Plain, "false"),
    k("toggleSprint", CONTROLS, "sprint", "On toggles, off holds.", Plain, "false"),
    k("touchscreen", CONTROLS, "touchscreen mode", "", Plain, "false"),
    k("chatVisibility", CHAT, "chat", "", Choice(&[("0", "shown"), ("1", "commands only"), ("2", "hidden")]), "0"),
    k("chatColors", CHAT, "colours", "", Plain, "true"),
    k("chatLinks", CHAT, "web links", "", Plain, "true"),
    k("chatLinksPrompt", CHAT, "prompt on links", "", Plain, "true"),
    k("chatOpacity", CHAT, "text opacity", "", Range(0.0, 1.0), "1.0"),
    k("textBackgroundOpacity", CHAT, "text background opacity", "", Range(0.0, 1.0), "0.5"),
    k("chatScale", CHAT, "text size", "", Range(0.0, 1.0), "1.0"),
    k("chatLineSpacing", CHAT, "line spacing", "", Range(0.0, 1.0), "0.0"),
    k("chatDelay", CHAT, "message delay", "Seconds.", Range(0.0, 6.0), "0.0"),
    k("chatWidth", CHAT, "width", "", Range(0.0, 1.0), "1.0"),
    k("chatHeightFocused", CHAT, "focused height", "", Range(0.0, 1.0), "1.0"),
    k("chatHeightUnfocused", CHAT, "unfocused height", "", Range(0.0, 1.0), "0.4375"),
    k("autoSuggestions", CHAT, "command suggestions", "", Plain, "true"),
    k("hideMatchedNames", CHAT, "hide matched names", "", Plain, "true"),
    k("onlyShowSecureChat", CHAT, "only show secure chat", "", Plain, "false"),
    k("narrator", ACCESS, "narrator", "", Choice(&[("0", "off"), ("1", "all"), ("2", "chat"), ("3", "system")]), "0"),
    k("highContrast", ACCESS, "high contrast", "", Plain, "false"),
    k("backgroundForChatOnly", ACCESS, "text background", "On: only behind chat. Off: everywhere.", Plain, "true"),
    k("notificationDisplayTime", ACCESS, "notification time", "", Range(0.5, 10.0), "1.0"),
    k("screenEffectScale", ACCESS, "distortion effects", "Nausea and nether portal warping.", Range(0.0, 1.0), "1.0"),
    k("fovEffectScale", ACCESS, "fov effects", "How much speed changes the field of view.", Range(0.0, 1.0), "1.0"),
    k("darknessEffectScale", ACCESS, "darkness pulsing", "", Range(0.0, 1.0), "1.0"),
    k("damageTiltStrength", ACCESS, "damage tilt", "", Range(0.0, 1.0), "1.0"),
    k("hideLightningFlashes", ACCESS, "hide lightning flashes", "", Plain, "false"),
    k("darkMojangStudiosBackground", ACCESS, "monochrome logo", "", Plain, "false"),
    k("panoramaScrollSpeed", ACCESS, "panorama speed", "", Range(0.0, 1.0), "1.0"),
    k("hideSplashTexts", ACCESS, "hide splash texts", "", Plain, "false"),
    k("forceUnicodeFont", ACCESS, "force unicode font", "", Plain, "false"),
    k("japaneseGlyphVariants", ACCESS, "japanese glyph variants", "", Plain, "false"),
    k("mainHand", SKIN, "main hand", "", Choice(&[("right", "right"), ("left", "left")]), "right"),
    k("modelPart_cape", SKIN, "cape", "", Plain, "true"),
    k("modelPart_jacket", SKIN, "jacket", "", Plain, "true"),
    k("modelPart_left_sleeve", SKIN, "left sleeve", "", Plain, "true"),
    k("modelPart_right_sleeve", SKIN, "right sleeve", "", Plain, "true"),
    k("modelPart_left_pants_leg", SKIN, "left pants leg", "", Plain, "true"),
    k("modelPart_right_pants_leg", SKIN, "right pants leg", "", Plain, "true"),
    k("modelPart_hat", SKIN, "hat", "", Plain, "true"),
    k("lang", GAME, "language", "", Plain, "en_us"),
    k("pauseOnLostFocus", GAME, "pause on lost focus", "Pause when the window loses focus (F3+P).", Plain, "true"),
    k("advancedItemTooltips", GAME, "advanced tooltips", "Item ids and durability in tooltips (F3+H).", Plain, "false"),
    k("realmsNotifications", GAME, "realms notifications", "", Plain, "true"),
    k("allowServerListing", GAME, "allow server listings", "", Plain, "true"),
    k("operatorItemsTab", GAME, "operator items tab", "", Plain, "false"),
];

/// Sections in the order the menus have them; key bindings and the rest come after.
const ORDER: &[&str] = &[VIDEO, SOUND, CONTROLS, "key bindings", CHAT, ACCESS, SKIN, GAME, "other"];

/// "key_key.forward" → "forward", "key_key.hotbar.1" → "hotbar 1", "key_journeymap.minimap" → "journeymap minimap".
fn binding_name(key: &str) -> String {
    let k = key.trim_start_matches("key_");
    let k = k.strip_prefix("key.").unwrap_or(k);
    k.replace(['.', '_'], " ").trim().to_string()
}

/// "key.keyboard.left.shift" → "left shift", "key.mouse.left" → "left mouse button".
pub fn binding_value(v: &str) -> Option<String> {
    if let Some(k) = v.strip_prefix("key.keyboard.") {
        return Some(if k == "unknown" { "not bound".into() } else { k.replace('.', " ") });
    }
    let b = v.strip_prefix("key.mouse.")?;
    Some(format!("{} mouse button", b.replace('.', " ")))
}

fn scalar(raw: &str) -> Scalar {
    match raw {
        "true" => return Scalar::Bool(true),
        "false" => return Scalar::Bool(false),
        _ => {}
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Scalar::Str(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    if !raw.contains(['.', 'e', 'E']) && let Ok(i) = raw.parse::<i64>() {
        return Scalar::Int(i);
    }
    match raw.parse::<f64>() {
        Ok(f) if f.is_finite() && raw.chars().any(|c| c.is_ascii_digit()) => Scalar::Float(f),
        _ => Scalar::Str(raw.to_string()),
    }
}

/// The settings in `text`, sectioned like the game's menus.
pub fn doc(text: &str) -> Doc {
    let mut fields = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let Some(colon) = body.find(':') else { continue };
        let key = &body[..colon];
        if key.is_empty() || key.contains(char::is_whitespace) || key == "version" {
            continue;
        }
        let raw = &body[colon + 1..];
        let span = start + colon + 1..start + body.len();
        let known = KNOWN.iter().find(|k| k.key == key);
        let mut hint = Hint { on_off: true, ..Default::default() };
        let mut value = if raw.starts_with('[') || raw.starts_with('{') { Value::Other(raw.to_string()) } else { Value::Scalar(scalar(raw)) };
        let (section, label, doc) = match known {
            Some(k) => {
                hint.default = Some(k.default.to_string());
                match k.kind {
                    Range(a, b) => {
                        hint.min = Some(a);
                        hint.max = Some(b);
                    }
                    Choice(c) => {
                        // Choices are matched as written (graphics is "1", clouds "true"), so offer them as text.
                        let as_text = match &value {
                            Value::Scalar(Scalar::Str(s)) => s.clone(),
                            Value::Scalar(s) => s.display(),
                            _ => String::new(),
                        };
                        value = Value::Scalar(Scalar::Str(as_text));
                        hint.allowed = c.iter().map(|(v, _)| v.to_string()).collect();
                        hint.labels = c.iter().map(|(v, n)| (v.to_string(), n.to_string())).collect();
                    }
                    Plain => {}
                }
                (k.section.to_string(), k.label.to_string(), k.doc.to_string())
            }
            None if key.starts_with("key_") => {
                // Clicked, then the key pressed: see the configs tab.
                hint.binding = true;
                let vanilla = BINDINGS.iter().find(|(k, _, _)| *k == key);
                hint.default = vanilla.map(|(_, _, d)| d.to_string());
                let label = vanilla.map_or_else(|| binding_name(key), |(_, l, _)| l.to_string());
                ("key bindings".to_string(), label, String::new())
            }
            None => ("other".to_string(), key.to_string(), String::new()),
        };
        let rank = KNOWN.iter().position(|k| k.key == key).or_else(|| BINDINGS.iter().position(|(k, _, _)| *k == key)).unwrap_or(usize::MAX);
        fields.push((ORDER.iter().position(|s| *s == section).unwrap_or(ORDER.len()), rank, Field { section: vec![section], key: key.to_string(), label: Some(label), value, span, doc, hint }));
    }
    // Menu order: by section, then as listed above (keys in the file's order otherwise).
    fields.sort_by_key(|(s, k, _)| (*s, *k));
    Doc { fields: fields.into_iter().map(|(_, _, f)| f).collect(), section_docs: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mod_config::{Format, set};

    const SAMPLE: &str = "version:3955\nautoJump:false\nrenderDistance:12\ngraphicsMode:1\nrenderClouds:\"true\"\nfov:0.0\nlang:en_us\nresourcePacks:[\"vanilla\"]\nkey_key.forward:key.keyboard.w\nsoundCategory_music:0.25\nsomeModOption:3\n";

    #[test]
    fn options_read_as_named_fields_in_menu_order() {
        let d = doc(SAMPLE);
        let f = |k: &str| d.fields.iter().find(|f| f.key == k).unwrap();
        assert!(d.fields.iter().all(|f| f.key != "version"), "the data version isn't a setting");
        assert_eq!(f("renderDistance").label.as_deref(), Some("render distance"));
        assert_eq!(f("renderDistance").hint.max, Some(32.0));
        assert_eq!(f("graphicsMode").value, Value::Scalar(Scalar::Str("1".into())));
        assert_eq!(f("graphicsMode").hint.name_of("1"), "fancy");
        assert_eq!(f("renderClouds").value, Value::Scalar(Scalar::Str("true".into())));
        assert!(matches!(f("resourcePacks").value, Value::Other(_)));
        assert_eq!(f("key_key.forward").section, ["key bindings"]);
        assert!(f("key_key.forward").hint.binding);
        assert_eq!(f("key_key.forward").label.as_deref(), Some("walk forwards"));
        assert_eq!(f("key_key.forward").hint.default.as_deref(), Some("key.keyboard.w"));
        assert_eq!(f("renderDistance").hint.default.as_deref(), Some("12"));
        assert_eq!(binding_value("key.keyboard.left.shift").as_deref(), Some("left shift"));
        assert_eq!(f("someModOption").section, ["other"]);
        let order: Vec<&str> = d.fields.iter().map(|f| f.section[0].as_str()).collect();
        assert_eq!(order.first(), Some(&"video"));
        assert_eq!(order.last(), Some(&"other"));
    }

    #[test]
    fn options_edits_touch_only_the_value() {
        let d = doc(SAMPLE);
        let f = |k: &str| d.fields.iter().find(|f| f.key == k).unwrap().clone();
        let t = set(Format::Options, SAMPLE, &f("renderDistance"), &Value::Scalar(Scalar::Int(16))).unwrap();
        assert_eq!(t, SAMPLE.replace("renderDistance:12", "renderDistance:16"));
        let t = set(Format::Options, SAMPLE, &f("renderClouds"), &Value::Scalar(Scalar::Str("fast".into()))).unwrap();
        assert_eq!(t, SAMPLE.replace("renderClouds:\"true\"", "renderClouds:\"fast\""), "quoted values stay quoted");
        let t = set(Format::Options, SAMPLE, &f("graphicsMode"), &Value::Scalar(Scalar::Str("2".into()))).unwrap();
        assert_eq!(t, SAMPLE.replace("graphicsMode:1", "graphicsMode:2"), "numbers chosen by name stay numbers");
        let t = set(Format::Options, SAMPLE, &f("autoJump"), &Value::Scalar(Scalar::Bool(true))).unwrap();
        assert_eq!(t, SAMPLE.replace("autoJump:false", "autoJump:true"));
    }
}
