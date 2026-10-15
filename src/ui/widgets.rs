//! Small widget kit in the Omarchy idiom: square edges, mono text, menu rows
//! drawn with the shell's [menu] selected-* tokens, accent only where it means
//! "this is the thing".

use egui::{Align2, Color32, CornerRadius, FontId, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, vec2};

use crate::theme::{self, Theme};

pub fn row_height(t: &Theme) -> f32 {
    t.body_size() + 16.0
}

/// What sits at the left edge of a row.
pub enum Lead<'a> {
    None,
    Glyph(&'a str),
    Image(egui::ImageSource<'a>),
}

fn row_frame(ui: &Ui, t: &Theme, rect: Rect, resp: &Response, selected: bool, tint: Option<Color32>) {
    let p = ui.painter();
    if let Some(c) = tint {
        // Group colour: a soft wash and border, stronger when selected or hovered.
        let (fill, border) = match (selected, resp.hovered()) {
            (true, _) => (0.22, 0.85),
            (false, true) => (0.14, 0.5),
            _ => (0.08, 0.3),
        };
        p.rect(rect, CornerRadius::ZERO, Theme::alpha(c, fill), Stroke::new(1.0, Theme::alpha(c, border)), StrokeKind::Inside);
    } else if selected {
        p.rect(
            rect,
            CornerRadius::ZERO,
            t.fg_a(t.menu_selected_bg_alpha),
            Stroke::new(1.0, t.fg_a(t.menu_selected_border_alpha)),
            StrokeKind::Inside,
        );
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::ZERO, t.fg_a(t.hover_fill_alpha * 0.6));
    }
}

/// Paints the lead in a `size` square at `x`; returns the x where text starts.
fn paint_lead(ui: &Ui, t: &Theme, lead: Lead, x: f32, cy: f32, size: f32, selected: bool, dim: bool) -> f32 {
    match lead {
        Lead::None => x,
        Lead::Glyph(g) => {
            let color = if selected && !dim { t.menu_selected_text } else { t.muted };
            ui.painter().text(egui::pos2(x, cy), Align2::LEFT_CENTER, g, FontId::monospace(t.body_size()), color);
            x + t.body_size() * 1.8
        }
        Lead::Image(src) => {
            let r = Rect::from_min_size(egui::pos2(x, cy - size / 2.0), vec2(size, size));
            let mut img = egui::Image::new(src).corner_radius(size * 0.18);
            if dim {
                img = img.tint(Color32::from_white_alpha(70));
            }
            img.paint_at(ui, r);
            x + size + 10.0
        }
    }
}

/// A full-width list row like Omarchy's launcher/menu entries.
pub fn menu_row(ui: &mut Ui, t: &Theme, selected: bool, icon: &str, left: &str, right: &str) -> Response {
    let lead = if icon.is_empty() { Lead::None } else { Lead::Glyph(icon) };
    lead_row(ui, t, selected, lead, left, right, None)
}

pub fn lead_row(ui: &mut Ui, t: &Theme, selected: bool, lead: Lead, left: &str, right: &str, tint: Option<Color32>) -> Response {
    lead_row_sense(ui, t, selected, lead, left, right, tint, Sense::click())
}

/// [`lead_row`] that can also be dragged.
pub fn drag_row(ui: &mut Ui, t: &Theme, selected: bool, lead: Lead, left: &str, right: &str, tint: Option<Color32>) -> Response {
    lead_row_sense(ui, t, selected, lead, left, right, tint, Sense::click_and_drag())
}

#[allow(clippy::too_many_arguments)]
fn lead_row_sense(ui: &mut Ui, t: &Theme, selected: bool, lead: Lead, left: &str, right: &str, tint: Option<Color32>, sense: Sense) -> Response {
    let h = row_height(t);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), sense);
    if ui.is_rect_visible(rect) {
        row_frame(ui, t, rect, &resp, selected, tint);
        let size = t.body_size();
        let color = if selected { t.menu_selected_text } else { t.fg };
        let x = paint_lead(ui, t, lead, rect.left() + 12.0, rect.center().y, h - 10.0, selected, false);
        let p = ui.painter();
        let right_w = if right.is_empty() {
            0.0
        } else {
            let g = p.layout_no_wrap(right.to_string(), FontId::monospace(size - 1.0), t.muted);
            let w = g.size().x;
            p.galley(egui::pos2(rect.right() - 12.0 - w, rect.center().y - g.size().y / 2.0), g, t.muted);
            w + 16.0
        };
        let clip = Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 12.0 - right_w, rect.bottom()));
        p.with_clip_rect(clip)
            .text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, left, FontId::monospace(size), color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Two-line row: a title over a muted subtitle, with an optional right label.
pub fn rich_row(ui: &mut Ui, t: &Theme, selected: bool, lead: Lead, title: &str, subtitle: &str, right: &str) -> Response {
    rich_row_dim(ui, t, selected, lead, title, subtitle, right, false)
}

/// [`rich_row`], greyed out when `dim` (e.g. a disabled mod).
#[allow(clippy::too_many_arguments)]
pub fn rich_row_dim(ui: &mut Ui, t: &Theme, selected: bool, lead: Lead, title: &str, subtitle: &str, right: &str, dim: bool) -> Response {
    let size = t.body_size();
    let h = size * 2.0 + 22.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if ui.is_rect_visible(rect) {
        row_frame(ui, t, rect, &resp, selected, None);
        let x = paint_lead(ui, t, lead, rect.left() + 10.0, rect.center().y, h - 14.0, selected, dim);
        let p = ui.painter();
        let right_w = if right.is_empty() {
            0.0
        } else {
            let g = p.layout_no_wrap(right.to_string(), FontId::monospace(size - 1.0), t.muted);
            let w = g.size().x;
            p.galley(egui::pos2(rect.right() - 12.0 - w, rect.center().y - g.size().y / 2.0), g, t.muted);
            w + 16.0
        };
        let clip = Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 12.0 - right_w, rect.bottom()));
        let p = p.with_clip_rect(clip);
        let color = match (dim, selected) {
            (true, _) => t.muted,
            (false, true) => t.menu_selected_text,
            (false, false) => t.fg_light,
        };
        let sub = if dim { Theme::alpha(t.muted, 0.6) } else { t.muted };
        p.text(egui::pos2(x, rect.center().y - 2.0), Align2::LEFT_BOTTOM, title, theme::bold(size), color);
        p.text(egui::pos2(x, rect.center().y + 2.0), Align2::LEFT_TOP, subtitle, FontId::monospace(size - 1.0), sub);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Accent bar on the selected row of the pane that has keyboard focus.
pub fn focus_mark(ui: &Ui, t: &Theme, rect: Rect) {
    let bar = Rect::from_min_size(rect.min, vec2(3.0, rect.height()));
    ui.painter().rect_filled(bar, CornerRadius::ZERO, t.accent);
}

/// Collapsible group header in a list.
pub fn group_row(ui: &mut Ui, t: &Theme, name: &str, count: usize, collapsed: bool, tint: Option<Color32>) -> Response {
    let size = t.body_size() - 1.0;
    let h = size + 14.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let color = match tint {
            Some(c) => c,
            None if resp.hovered() => t.fg,
            None => t.muted,
        };
        let arrow = if collapsed { "\u{f44a}" } else { "\u{f44b}" };
        let g = p.layout_no_wrap(format!("{arrow} {name}"), FontId::monospace(size), color);
        let tw = g.size().x;
        p.galley(egui::pos2(rect.left() + 6.0, rect.center().y - g.size().y / 2.0), g, color);
        let c = p.layout_no_wrap(count.to_string(), FontId::monospace(size), t.muted);
        let cw = c.size().x;
        p.galley(egui::pos2(rect.right() - 12.0 - cw, rect.center().y - c.size().y / 2.0), c, t.muted);
        let (l, r) = (rect.left() + 14.0 + tw, rect.right() - 20.0 - cw);
        if r > l {
            let line = tint.map(|c| Theme::alpha(c, 0.35)).unwrap_or(t.fg_a(0.12));
            p.hline(l..=r, rect.center().y, Stroke::new(1.0, line));
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Text tabs with an accent underline, wrapping onto more lines when narrow.
pub fn tab_strip<T: PartialEq + Copy>(ui: &mut Ui, t: &Theme, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        // Room under each line for the active tab's underline.
        ui.spacing_mut().item_spacing = vec2(16.0, 14.0);
        for (v, label) in options {
            let on = *value == *v;
            let color = if on { t.fg_light } else { t.muted };
            let r = ui.add(egui::Label::new(RichText::new(*label).color(color)).sense(Sense::click()));
            if on {
                ui.painter().hline(r.rect.x_range(), r.rect.bottom() + 4.0, Stroke::new(2.0, t.accent));
            }
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                *value = *v;
                changed = true;
            }
        }
    });
    let r = ui.available_rect_before_wrap();
    ui.add_space(8.0);
    ui.painter().hline(r.x_range(), r.top() + 4.0, Stroke::new(1.0, t.fg_a(0.12)));
    ui.add_space(8.0);
    changed
}

/// Centered glyph, title and hint for an empty tab.
pub fn empty_state(ui: &mut Ui, t: &Theme, glyph: &str, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.25).min(80.0));
        ui.label(RichText::new(glyph).size(t.body_size() * 3.0).color(t.muted));
        ui.add_space(6.0);
        ui.label(RichText::new(title).color(t.fg));
        if !hint.is_empty() {
            para(ui, RichText::new(hint).color(t.muted));
        }
    });
}

/// The one filled button on a screen.
pub fn primary_button(ui: &mut Ui, t: &Theme, label: &str) -> Response {
    let text = RichText::new(label).color(t.bg).font(theme::bold(t.body_size()));
    let fill = t.accent;
    ui.add(egui::Button::new(text).fill(fill).stroke(Stroke::new(1.0, fill)).corner_radius(0))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn button(ui: &mut Ui, label: &str) -> Response {
    ui.add(egui::Button::new(label)).on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn danger_button(ui: &mut Ui, t: &Theme, label: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(label).color(t.red)).stroke(Stroke::new(1.0, Theme::alpha(t.red, 0.6))))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Segmented choice, e.g. vanilla / fabric / quilt.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, t: &Theme, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (v, label) in options {
            let on = *value == *v;
            let text = if on { RichText::new(*label).color(t.bg) } else { RichText::new(*label).color(t.fg) };
            let mut b = egui::Button::new(text).corner_radius(0);
            if on {
                b = b.fill(t.accent).stroke(Stroke::new(1.0, t.accent));
            }
            if ui.add(b).clicked() && !on {
                *value = *v;
                changed = true;
            }
        }
    });
    changed
}

/// Muted lowercase label with a hairline, separating sections.
pub fn section(ui: &mut Ui, t: &Theme, title: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).color(t.muted).size(t.body_size() - 1.0));
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, t.fg_a(0.12)));
    });
    ui.add_space(2.0);
}

pub fn kv(ui: &mut Ui, t: &Theme, key: &str, value: impl Into<RichText>) {
    ui.horizontal(|ui| {
        field_label(ui, t, key);
        ui.label(value.into());
    });
}

/// Fixed-width, left-aligned muted label for form rows.
pub fn field_label(ui: &mut Ui, t: &Theme, key: &str) {
    let w = t.body_size() * 9.0;
    ui.allocate_ui_with_layout(vec2(w, t.body_size() + 4.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        ui.label(RichText::new(key).color(t.muted));
    });
}

/// `key label` hint for the bottom bar.
pub fn key_hint(ui: &mut Ui, t: &Theme, key: &str, label: &str) {
    let size = t.body_size() - 1.0;
    let g = ui.painter().layout_no_wrap(key.to_string(), FontId::monospace(size), t.fg_light);
    let (rect, _) = ui.allocate_exact_size(g.size() + vec2(10.0, 4.0), Sense::hover());
    ui.painter().rect(rect, CornerRadius::ZERO, t.fg_a(t.normal_fill_alpha * 2.0), Stroke::new(1.0, t.fg_a(0.18)), StrokeKind::Inside);
    ui.painter().galley(rect.min + vec2(5.0, 2.0), g, t.fg_light);
    ui.label(RichText::new(label).color(t.muted).size(size));
    ui.add_space(8.0);
}

/// Thin progress bar, accent on a faint track.
pub fn progress(ui: &mut Ui, t: &Theme, frac: Option<f32>, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 4.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, t.fg_a(0.10));
    match frac {
        Some(f) => {
            let mut r = rect;
            r.set_width(rect.width() * f.clamp(0.0, 1.0));
            p.rect_filled(r, CornerRadius::ZERO, t.accent);
        }
        None => {
            // Indeterminate: a sliding block.
            let time = ui.input(|i| i.time) as f32;
            let w = rect.width() * 0.25;
            let x = rect.left() + ((time * 0.8).fract() * (rect.width() + w)) - w;
            let r = Rect::from_min_size(egui::pos2(x.max(rect.left()), rect.top()), vec2(w.min(rect.right() - x.max(rect.left())), rect.height()));
            p.rect_filled(r, CornerRadius::ZERO, t.accent);
            ui.ctx().request_repaint();
        }
    }
}

pub fn modal_frame(t: &Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(t.bg)
        .stroke(Stroke::new(2.0, t.border_active))
        .inner_margin(20)
        .corner_radius(0)
}

pub fn scrim(t: &Theme) -> Color32 {
    Theme::alpha(t.bg, t.scrim_alpha)
}

pub fn heading(ui: &mut Ui, t: &Theme, text: &str) {
    ui.label(RichText::new(text).font(theme::bold(t.body_size() * 1.6)).color(t.fg_light));
}

pub fn size_label(n: u64) -> String {
    let n = n as f64;
    match n {
        n if n >= 1e9 => format!("{:.1} GB", n / 1e9),
        n if n >= 1e6 => format!("{:.1} MB", n / 1e6),
        n if n >= 1e3 => format!("{:.0} kB", n / 1e3),
        n => format!("{n} B"),
    }
}

pub fn ago(when: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let Some(w) = when else { return "never".into() };
    let s = (chrono::Utc::now() - w).num_seconds().max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// A paragraph: wraps to the available width (labels clip by default).
pub fn para(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    // Not justified, even inside columns (which justify by default).
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| ui.add(egui::Label::new(text).wrap())).inner
}

/// Text input that grabs focus when `focus` is set.
pub fn text_input(ui: &mut Ui, value: &mut String, hint: &str, width: f32, focus: bool) -> Response {
    let r = ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(width).margin(vec2(8.0, 5.0)));
    if focus {
        r.request_focus();
    }
    r
}
