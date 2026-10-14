//! The expanding search pill (Lacrima's `AlbumSearch` TextBox): a
//! rounded pill collapsed to a magnifier that grows to its full width
//! over 0.35s CubicEaseOut while hovered/focused, then shrinks back.
//!
//! Two specs share the machinery:
//! - `ui` — the 35px top-right pill, expands *leftward* from its
//!   right-docked anchor.
//! - `ui_band` — a bigger pill docked in the media-buttons band;
//!   hovering swells the icon into the box's magnifier while the box
//!   expands *rightward* from it. Both collapse to a bordered circle
//!   around the icon.

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};

const OPEN_SECS: f32 = 0.35;
const FADE_SECS: f32 = 0.3;

struct Spec {
    w_min: f32,
    w_max: f32,
    h: f32,
    radius: f32,
    font: f32,
    icon_d: f32,
    icon_left: f32,
    /// Icon→text margin inside the open box.
    text_gap: f32,
    /// Expand rightward from a left anchor (band) vs leftward from a
    /// right anchor (top-right pill).
    grow_right: bool,
}

const PILL: Spec = Spec {
    w_min: 35.0,
    w_max: 220.0,
    h: 35.0,
    radius: 18.0,
    font: 12.5,
    icon_d: 16.0,
    icon_left: 10.0,
    text_gap: 5.0,
    grow_right: false,
};

const BAND: Spec = Spec {
    // w_min == h: the shrunken border is a perfect circle around the
    // collapsed icon — 50px tracks the album-toggle's glyph mass
    // (its bar + square span ~73 of the 100-unit canvas ≈ 44px of
    // the 60px slot) so both read the same visual height.
    w_min: 50.0,
    w_max: 300.0,
    h: 50.0,
    radius: 25.0,
    font: 15.0,
    icon_d: 25.0,
    icon_left: 12.5,
    text_gap: 11.0,
    grow_right: true,
};

#[derive(Default)]
pub struct SearchPill {
    pub text: String,
    /// Last frame's open state — drives the width animation.
    expanded: bool,
    focused: bool,
    /// `open` requested the field — focuses the TextEdit as soon as
    /// it exists (it's only created while the pill is expanding).
    want_focus: bool,
    edit_id: Option<egui::Id>,
}

impl SearchPill {
    /// Ctrl+F: expand the pill and focus its text field once it
    /// exists.
    pub fn open(&mut self) {
        self.expanded = true;
        self.want_focus = true;
    }

    /// Visibly open (or about to be) — Esc should close this rather
    /// than navigating back or leaving fullscreen.
    pub fn is_open(&self) -> bool {
        self.expanded || self.focused || self.want_focus
    }

    /// A Ctrl+F focus request is still in flight — the stray-focus
    /// clearing must not steal it before the edit widget exists.
    pub fn wants_focus(&self) -> bool {
        self.want_focus
    }

    /// Esc on an open pill: clear, defocus, collapse.
    pub fn close(&mut self, ctx: &egui::Context) {
        self.text.clear();
        self.expanded = false;
        self.focused = false;
        self.want_focus = false;
        if let Some(id) = self.edit_id.take() {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
    }

    /// `right`/`top` anchor the pill's top-right corner (Lacrima docks
    /// it HorizontalAlignment=Right Margin 0,15,30,0). `active` fades
    /// the pill in/out; `forced_open` keeps it expanded regardless of
    /// hover. Expands leftward.
    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        right: f32,
        top: f32,
        hint: &str,
        active: bool,
        forced_open: bool,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) {
        let spec = &PILL;
        let id = ui.id().with("search-pill").with(hint);
        let avail =
            ui.ctx().animate_bool_with_time(id.with("act"), active, FADE_SECS);
        if avail < 0.01 {
            self.focused = false;
            self.expanded = false;
            self.want_focus = false;
            return;
        }
        let alpha = alpha * avail;
        let t = self.open_t(ui, id);
        let w = (spec.w_min + (spec.w_max - spec.w_min) * t) * s;
        let rect = Rect::from_min_max(
            Pos2::new(right - w, top),
            Pos2::new(right, top + spec.h * s),
        );
        self.track_expand(ui, rect, interactive, forced_open);
        self.draw(ui, id, rect, t, alpha, interactive, s, spec, hint);
    }

    /// The media-buttons band variant: `icon_center` is where the
    /// collapsed magnifier sits (centered on its slot). The box grows
    /// rightward from there — icon fades out as a slightly bigger one
    /// fades in with the box; on close the icon fades back.
    #[allow(clippy::too_many_arguments)]
    pub fn ui_band(
        &mut self,
        ui: &mut egui::Ui,
        icon_center: Pos2,
        hint: &str,
        active: bool,
        forced_open: bool,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) {
        let spec = &BAND;
        let id = ui.id().with("search-pill").with(hint);
        let avail =
            ui.ctx().animate_bool_with_time(id.with("act"), active, FADE_SECS);
        if avail < 0.01 {
            self.focused = false;
            self.expanded = false;
            self.want_focus = false;
            return;
        }
        let alpha = alpha * avail;
        let t = self.open_t(ui, id);
        let w = (spec.w_min + (spec.w_max - spec.w_min) * t) * s;
        // Left anchor: the icon slot's left edge, so the icon stays
        // put while the box grows to its right.
        let left = icon_center.x - (spec.icon_left + spec.icon_d * 0.5) * s;
        let rect = Rect::from_min_max(
            Pos2::new(left, icon_center.y - spec.h * 0.5 * s),
            Pos2::new(left + w, icon_center.y + spec.h * 0.5 * s),
        );
        self.track_expand(ui, rect, interactive, forced_open);
        self.draw(ui, id, rect, t, alpha, interactive, s, spec, hint);
    }

    /// Width animation time (0 collapsed .. 1 open), eased.
    fn open_t(&mut self, ui: &egui::Ui, id: egui::Id) -> f32 {
        ease_out_cubic(ui.ctx().animate_bool_with_time(
            id.with("t"),
            self.expanded,
            OPEN_SECS,
        ))
    }

    /// Pointer-inside test, not a hovered widget: the TextEdit inside
    /// the expanded rect claims hover for itself, which would flip
    /// `expanded` false->true->false every frame (flicker).
    fn track_expand(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        interactive: bool,
        forced_open: bool,
    ) {
        let hovered = ui
            .ctx()
            .input(|i| i.pointer.latest_pos())
            .is_some_and(|p| rect.contains(p));
        self.expanded = interactive
            && (hovered
                || self.focused
                || forced_open
                || self.want_focus
                || !self.text.is_empty());
        if hovered && !self.expanded {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        rect: Rect,
        t: f32,
        alpha: f32,
        interactive: bool,
        s: f32,
        spec: &Spec,
        hint: &str,
    ) {
        let painter = ui.painter();
        let r = CornerRadius::same((spec.radius * s).round() as u8);
        painter.rect_filled(
            rect,
            r,
            Color32::from_black_alpha((0xAA as f32 * t * alpha) as u8),
        );
        // Border: always drawn — shrunk to w_min==h it reads as a
        // stroked circle around the collapsed icon (same treatment as
        // the band pill and its album-toggle sibling). The top-right
        // ring is proportionally lighter and eases to its thin open
        // border as the box grows.
        if spec.grow_right {
            painter.rect_stroke(
                rect,
                r,
                Stroke::new(
                    3.5 * s,
                    Color32::from_white_alpha((0xB0 as f32 * alpha) as u8),
                ),
                StrokeKind::Inside,
            );
        } else {
            // Collapsed face sits dimmed — ~50% — brightening to full
            // as the box opens.
            let dim = 0.5 + 0.5 * t;
            painter.rect_stroke(
                rect,
                r,
                Stroke::new(
                    (2.5 - 1.5 * t) * s,
                    Color32::from_white_alpha(
                        ((0xB0 as f32 - (0xB0 - 0x44) as f32 * t) * alpha * dim)
                            as u8,
                    ),
                ),
                StrokeKind::Inside,
            );
        }

        // Magnifier, docked left (InnerLeftContent, Margin 10,0,5,0).
        let ic = Pos2::new(
            rect.min.x + (spec.icon_left + spec.icon_d * 0.5) * s,
            rect.center().y,
        );
        let k = spec.icon_d * s / 24.0;
        if spec.grow_right {
            // One continuous icon — lit white like the sibling glyphs,
            // swelling ~15% into the box's version as it opens.
            magnifier(
                painter,
                ic,
                k * (1.0 + 0.15 * t),
                Color32::from_white_alpha((0xF0 as f32 * alpha) as u8),
            );
        } else {
            magnifier(
                painter,
                ic,
                k,
                Color32::from_white_alpha(
                    (0xCC as f32 * alpha * (0.5 + 0.5 * t)) as u8,
                ),
            );
        }

        // Text field + clear button while open (Lacrima fades them with
        // the same transition).
        if t > 0.05 && interactive {
            let clear = !self.text.is_empty();
            let clear_rect = Rect::from_center_size(
                Pos2::new(rect.max.x - 15.0 * s, rect.center().y),
                Vec2::splat(16.0 * s),
            );
            if clear && t > 0.6 {
                let c = Color32::from_white_alpha((0xAA as f32 * alpha) as u8);
                let g = 3.5 * s;
                painter.line_segment(
                    [clear_rect.min + Vec2::splat(g), clear_rect.max - Vec2::splat(g)],
                    Stroke::new(1.6 * s, c),
                );
                painter.line_segment(
                    [
                        Pos2::new(clear_rect.max.x - g, clear_rect.min.y + g),
                        Pos2::new(clear_rect.min.x + g, clear_rect.max.y - g),
                    ],
                    Stroke::new(1.6 * s, c),
                );
            }
            let edit_right = rect.max.x - if clear { 30.0 } else { 8.0 } * s;
            let line_h = spec.font * s * 1.4;
            let inner = Rect::from_min_max(
                Pos2::new(
                    rect.min.x + (spec.icon_left + spec.icon_d + spec.text_gap) * s,
                    rect.center().y - line_h * 0.5,
                ),
                Pos2::new(edit_right, rect.center().y + line_h * 0.5),
            );
            let mut text = std::mem::take(&mut self.text);
            ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
                // hint_text resolves TextStyle::Body from the Ui style
                // (it ignores TextEdit::font), so pin both styles.
                ui.style_mut()
                    .text_styles
                    .insert(egui::TextStyle::Body, FontId::proportional(spec.font * s));
                ui.style_mut()
                    .text_styles
                    .insert(
                        egui::TextStyle::Monospace,
                        FontId::proportional(spec.font * s),
                    );
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .id(id.with("edit"))
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(spec.font * s))
                        .text_color(Color32::WHITE.gamma_multiply(alpha))
                        .hint_text(hint)
                        .frame(egui::Frame::NONE),
                );
                self.edit_id = Some(edit.id);
                if self.want_focus {
                    if edit.has_focus() {
                        self.want_focus = false;
                    } else {
                        ui.ctx().memory_mut(|m| m.request_focus(edit.id));
                    }
                }
                self.focused = edit.has_focus();
            });
            self.text = text;
            if clear {
                let clr = ui.interact(clear_rect, id.with("clr"), Sense::click());
                if clr.clicked() {
                    self.text.clear();
                }
                if clr.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }
        } else {
            self.focused = false;
        }
    }
}

/// The magnifier glyph at `k` = size/24 scale.
fn magnifier(painter: &egui::Painter, ic: Pos2, k: f32, c: Color32) {
    painter.circle_stroke(
        ic + Vec2::new(-0.5 * k, -0.5 * k),
        6.5 * k,
        Stroke::new(2.0 * k, c),
    );
    painter.line_segment(
        [
            ic + Vec2::new(4.4 * k, 4.4 * k),
            ic + Vec2::new(9.2 * k, 9.2 * k),
        ],
        Stroke::new(2.4 * k, c),
    );
}

/// CubicEaseOut (Lacrima's width transition).
fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}
