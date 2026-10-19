//! The "Add Url" / "Add Playlist" prompt — a port of Lacrima's
//! `AddUrlPopupContent`/`AddPlaylistPopupContent` overlays: dimmed
//! scrim, centered title + rounded textbox, 0.45s CubicEaseOut fade.
//! Enter and Esc both dismiss (Lacrima's Submit commands close the
//! popup either way); the caller reads the text on close and adds the
//! item(s) when it's non-empty.

use eframe::egui::{self, Align2, Color32, FontId, Key, Pos2, Rect, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Url,
    Playlist,
}

pub struct UrlPrompt {
    kind: Kind,
    text: String,
    /// True while the overlay exists on screen (in or out fade).
    open: bool,
    /// Visibility target — false during the dismiss fade.
    shown: bool,
    /// One-shot focus request for the freshly opened textbox.
    want_focus: bool,
    /// Set while a playlist expansion is in flight (Lacrima's
    /// IsAddingPlaylist) — blocks re-entry.
    pub playlist_busy: bool,
}

impl UrlPrompt {
    pub fn new() -> Self {
        Self {
            kind: Kind::Url,
            text: String::new(),
            open: false,
            shown: false,
            want_focus: false,
            playlist_busy: false,
        }
    }

    pub fn open(&mut self, kind: Kind, ctx: &egui::Context) {
        if self.playlist_busy && kind == Kind::Playlist {
            return;
        }
        self.kind = kind;
        self.text.clear();
        self.open = true;
        self.shown = true;
        self.want_focus = true;
        // animate_bool jumps to its target on first call — seed the id
        // at 0 so the first painted frame actually fades in.
        ctx.animate_bool(egui::Id::new("url-prompt-fade"), false);
    }

    /// Hit-test gating: nothing behind the scrim may receive input
    /// while the prompt is up (same rule as the tag editor).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Start the dismiss fade (back button) — the pending text is
    /// still submitted on close, like Enter/Esc.
    pub fn dismiss(&mut self) {
        self.shown = false;
    }

    /// Paint + input. Returns `Some((kind, url))` when the prompt
    /// dismisses with non-empty text — the submitted link to add.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        content: Rect,
        s: f32,
    ) -> Option<(Kind, String)> {
        if !self.open {
            return None;
        }
        let ctx = ui.ctx().clone();
        // Same 0.45s CubicEaseOut as Lacrima's popup opacity
        // transition.
        let id = egui::Id::new("url-prompt-fade");
        let fade = ctx.animate_bool_with_time_and_easing(
            id,
            self.shown,
            0.45,
            egui::emath::easing::cubic_out,
        );
        if fade <= 0.001 && !self.shown {
            self.open = false;
            let text = self.text.trim().to_string();
            return (!text.is_empty()).then(|| (self.kind, text));
        }
        ctx.request_repaint();

        // Scrim — swallows pointer input over the whole content area
        // (Lacrima's popup grid blocks hit-testing the same way; it
        // does not close on click, only Enter/Esc).
        let painter = ui.painter().clone();
        painter.rect_filled(
            content,
            egui::CornerRadius::ZERO,
            Color32::from_black_alpha(230).gamma_multiply(fade),
        );
        let _ = ui.interact(content, ui.id().with("url-prompt-scrim"), egui::Sense::click());

        // Centered column: "Enter Url" + the rounded textbox.
        let (title, hint, icon_list) = match self.kind {
            Kind::Url => ("Enter Url", "Enter Url here", false),
            Kind::Playlist => ("Enter Url", "Enter Playlist Url here", true),
        };
        let box_w = 450.0 * s;
        let box_h = 45.0 * s;
        let title_h = 34.0 * s;
        let gap = 16.0 * s;
        let total_h = title_h + gap + box_h;
        let cy = content.center().y - total_h * 0.5;

        painter.text(
            Pos2::new(content.center().x, cy + title_h * 0.5),
            Align2::CENTER_CENTER,
            title,
            FontId::proportional(26.0 * s),
            Color32::WHITE.gamma_multiply(0.92 * fade),
        );

        let box_rect = Rect::from_center_size(
            Pos2::new(content.center().x, cy + title_h + gap + box_h * 0.5),
            Vec2::new(box_w, box_h),
        );
        let bg = Color32::from_black_alpha(204).gamma_multiply(fade);
        let border = Color32::from_white_alpha(68).gamma_multiply(fade);
        painter.rect_filled(box_rect, box_h * 0.5, bg);
        painter.rect_stroke(
            box_rect,
            box_h * 0.5,
            Stroke::new(1.5 * s, border),
            egui::StrokeKind::Inside,
        );

        // Left icon — monitor+triangle for urls, list+triangle for
        // playlists (Lacrima's InnerLeftContent glyphs).
        let ic = Pos2::new(box_rect.min.x + 22.0 * s, box_rect.center().y);
        let icol = Color32::from_white_alpha(150).gamma_multiply(fade);
        let iw = 15.0 * s;
        if icon_list {
            for i in 0..3 {
                let y = ic.y + (i as f32 - 1.0) * 5.0 * s;
                painter.line_segment(
                    [
                        Pos2::new(ic.x - iw * 0.5, y),
                        Pos2::new(ic.x - iw * 0.5 + 3.0 * s, y),
                    ],
                    Stroke::new(1.4 * s, icol),
                );
                painter.line_segment(
                    [
                        Pos2::new(ic.x - iw * 0.5 + 5.0 * s, y),
                        Pos2::new(ic.x + iw * 0.5, y),
                    ],
                    Stroke::new(1.4 * s, icol),
                );
            }
        } else {
            painter.rect_stroke(
                Rect::from_center_size(
                    Pos2::new(ic.x, ic.y - 1.0 * s),
                    Vec2::new(iw, iw * 0.72),
                ),
                1.5 * s,
                Stroke::new(1.4 * s, icol),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    Pos2::new(ic.x - 4.0 * s, ic.y + 4.5 * s),
                    Pos2::new(ic.x + 4.0 * s, ic.y + 4.5 * s),
                ],
                Stroke::new(1.4 * s, icol),
            );
        }
        // Play triangle inside the icon.
        let t = 4.0 * s;
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(ic.x - t * 0.45, ic.y - 1.0 * s - t * 0.7),
                Pos2::new(ic.x - t * 0.45, ic.y - 1.0 * s + t * 0.7),
                Pos2::new(ic.x + t * 0.75, ic.y - 1.0 * s),
            ],
            icol,
            Stroke::NONE,
        ));

        // Textbox — frameless; the pill above is the visual. Left
        // padding clears the icon. The child ui's max_rect is itself
        // text-height centered on the box (the search_pill pattern —
        // a full-height rect leaves the single-line galley top-biased
        // no matter the row alignment).
        let line_h = 16.0 * s * 1.4;
        let text_rect = Rect::from_min_max(
            Pos2::new(
                box_rect.min.x + 42.0 * s,
                box_rect.center().y - line_h * 0.5,
            ),
            Pos2::new(
                box_rect.max.x - 16.0 * s,
                box_rect.center().y + line_h * 0.5,
            ),
        );
        let mut tui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        tui.visuals_mut().override_text_color =
            Some(Color32::WHITE.gamma_multiply(0.92 * fade));
        tui.visuals_mut().widgets.inactive.bg_stroke = Stroke::NONE;
        tui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        tui.visuals_mut().widgets.hovered.weak_bg_fill = Color32::TRANSPARENT;
        tui.visuals_mut().widgets.active.weak_bg_fill = Color32::TRANSPARENT;
        // hint_text resolves TextStyle::Body, not TextEdit::font —
        // pin it so the hint renders at the same size.
        tui.style_mut()
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(16.0 * s));
        let resp = tui.add(
            egui::TextEdit::singleline(&mut self.text)
                .hint_text(hint)
                .font(FontId::proportional(16.0 * s))
                .text_color(Color32::WHITE.gamma_multiply(0.92 * fade))
                .desired_width(text_rect.width() - 4.0 * s)
                .frame(egui::Frame::NONE),
        );
        if self.want_focus {
            resp.request_focus();
            self.want_focus = false;
        }
        // Enter submits; Esc dismisses (Lacrima's popup processes the
        // text on close either way). The singleline TextEdit surrenders
        // focus on Enter — nothing else can take focus while the modal
        // is up, so lost_focus IS the submit; the overlay-level key read
        // covers Esc/Enter when the box isn't focused.
        if resp.lost_focus()
            || ctx.input(|i| {
                i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape)
            })
        {
            self.shown = false;
        }
        None
    }
}
