//! The expanding search pill (Lacrima's `AlbumSearch` TextBox): a 35x35
//! rounded pill collapsed to a magnifier that grows leftward to 220px
//! over 0.35s CubicEaseOut while hovered/focused, then shrinks back.

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};

const W_MIN: f32 = 35.0;
const W_MAX: f32 = 220.0;
const H: f32 = 35.0;
const RADIUS: f32 = 18.0;
const OPEN_SECS: f32 = 0.35;
const FADE_SECS: f32 = 0.3;
const FONT: f32 = 12.5;
const ICON_D: f32 = 16.0;
const ICON_LEFT: f32 = 10.0;

#[derive(Default)]
pub struct SearchPill {
    pub text: String,
    /// Last frame's open state — drives the width animation.
    expanded: bool,
    focused: bool,
}

impl SearchPill {
    /// `right`/`top` anchor the pill's top-right corner (Lacrima docks
    /// it HorizontalAlignment=Right Margin 0,15,30,0). `active` fades
    /// the pill in/out (the band toggle reveals the album-filter pill);
    /// `forced_open` keeps it expanded regardless of hover.
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
        let id = ui.id().with("search-pill").with(hint);
        let avail = ui.ctx().animate_bool_with_time(id.with("act"), active, FADE_SECS);
        if avail < 0.01 {
            self.focused = false;
            self.expanded = false;
            return;
        }
        let alpha = alpha * avail;
        let t = ease_out_cubic(ui.ctx().animate_bool_with_time(
            id.with("t"),
            self.expanded,
            OPEN_SECS,
        ));
        let w = (W_MIN + (W_MAX - W_MIN) * t) * s;
        let rect = Rect::from_min_max(
            Pos2::new(right - w, top),
            Pos2::new(right, top + H * s),
        );

        // Pointer-inside test, not a hovered widget: the TextEdit inside
        // the expanded rect claims hover for itself, which would flip
        // `expanded` false->true->false every frame (flicker).
        // the expanded rect claims hover for itself, which would flip
        // `expanded` false->true->false every frame (flicker).
        let hovered = ui
            .ctx()
            .input(|i| i.pointer.latest_pos())
            .is_some_and(|p| rect.contains(p));
        self.expanded = interactive
            && (hovered || self.focused || forced_open || !self.text.is_empty());
        if hovered && !self.expanded {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let painter = ui.painter();
        let r = CornerRadius::same((RADIUS * s).round() as u8);
        painter.rect_filled(
            rect,
            r,
            Color32::from_black_alpha((0xAA as f32 * t * alpha) as u8),
        );
        painter.rect_stroke(
            rect,
            r,
            Stroke::new(
                1.0,
                Color32::from_white_alpha((0x44 as f32 * t * alpha) as u8),
            ),
            StrokeKind::Inside,
        );

        // Magnifier, docked left (InnerLeftContent, Margin 10,0,5,0).
        let ic = Pos2::new(rect.min.x + (ICON_LEFT + ICON_D * 0.5) * s, rect.center().y);
        let k = ICON_D * s / 24.0;
        let c = Color32::from_white_alpha((0xCC as f32 * alpha) as u8);
        painter.circle_stroke(ic + Vec2::new(-0.5 * k, -0.5 * k), 6.5 * k, Stroke::new(2.0 * k, c));
        painter.line_segment(
            [ic + Vec2::new(4.4 * k, 4.4 * k), ic + Vec2::new(9.2 * k, 9.2 * k)],
            Stroke::new(2.4 * k, c),
        );

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
            let line_h = FONT * s * 1.4;
            let inner = Rect::from_min_max(
                Pos2::new(
                    rect.min.x + (ICON_LEFT + ICON_D + 5.0) * s,
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
                    .insert(egui::TextStyle::Body, FontId::proportional(FONT * s));
                ui.style_mut()
                    .text_styles
                    .insert(egui::TextStyle::Monospace, FontId::proportional(FONT * s));
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .id(id.with("edit"))
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(FONT * s))
                        .text_color(Color32::WHITE.gamma_multiply(alpha))
                        .hint_text(hint)
                        .frame(egui::Frame::NONE),
                );
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

/// CubicEaseOut (Lacrima's width transition).
fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}
