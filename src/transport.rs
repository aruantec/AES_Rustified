//! The 140px media-buttons band above the waveform bar (Lacrima's
//! MusicView.axaml "Media buttons" grid): album-list toggle at the left,
//! stop/prev/play/next/repeat centered, cover-layout + equalizer +
//! vertical volume slider at the right. Icons are redrawn from the same
//! 100x100 canvases the Avalonia buttons use.

use std::collections::HashMap;
use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind,
    Vec2,
};

use crate::audio::{AudioPlayer, PlayerState};

// Button metrics (MusicView.axaml, base units scaled by `s`).
const SIDE_MARGIN: f32 = 50.0;
const CENTER_SPACING: f32 = 30.0;
const RIGHT_SPACING: f32 = 50.0;
const BTN: f32 = 60.0;
const PLAY_BTN: f32 = 100.0;
const KNOB_D: f32 = 100.0;

/// Left-cluster extent (base px): margin + list + gap + search slot.
const LEFT_EXTENT: f32 = SIDE_MARGIN + BTN + RIGHT_SPACING + BTN;
/// Half the centered transport cluster (stop/prev/play/next/repeat
/// plus the four gaps) — each side of the band center must fit this
/// plus its side cluster's extent. Pub so the caller can bound the
/// expanding search pill at the cluster's left edge.
pub(crate) const CENTER_HALF: f32 = (BTN * 4.0 + PLAY_BTN + CENTER_SPACING * 4.0) * 0.5;
/// Right-cluster extent (base px): margin + knob + gap + eq +
/// gap + layout — the wider of the two side clusters.
const RIGHT_EXTENT: f32 = SIDE_MARGIN + KNOB_D + (RIGHT_SPACING + BTN) * 2.0;
/// Edge-to-edge span the full row really needs (base px): the
/// centered cluster must clear the wider side cluster, so
/// `w/2 >= CENTER_HALF + max(side extents)` — below it icons
/// overlap and the band collapses to the compact strip. Derived
/// from the layout constants so the threshold is the exact
/// collision boundary, not an estimate.
const FULL_SPAN: f32 = 2.0
    * (CENTER_HALF + if LEFT_EXTENT > RIGHT_EXTENT { LEFT_EXTENT } else { RIGHT_EXTENT });
/// Full<->compact crossfade.
const COMPACT_SECS: f32 = 0.22;
/// Compact strip side margin (the 50px SIDE_MARGIN is a luxury of
/// the full layout).
const C_PAD: f32 = 16.0;

/// The kebab's reveal drawer height (base px) — a full button row
/// plus breathing room, laid out like the EQ drawer.
pub const OVF_H: f32 = 96.0;

const WHITE: Color32 = Color32::WHITE;
const LIT: Color32 = Color32::from_rgb(0xF0, 0xF0, 0xF0);
const DIM: Color32 = Color32::from_rgb(0x69, 0x69, 0x69);

// Volume knob: rotary dial — cyan level arc over a 270° sweep with a
// dead zone at the bottom (screen coords: 0° = right, 90° = down).
const KNOB_START: f32 = 135.0;
const KNOB_SWEEP: f32 = 270.0;
const KNOB_RING: Color32 = Color32::from_rgb(0x15, 0x17, 0x1B);
const KNOB_RING_EDGE: Color32 = Color32::from_rgb(0x2B, 0x2E, 0x33);
const KNOB_TRACK: Color32 = Color32::from_rgb(0x28, 0x2C, 0x31);

/// Button press bounce: deflate then re-inflate along a sine lobe so a
/// quick click still completes the whole squash, not just the press.
const BOUNCE_SECS: f32 = 0.32;
const BOUNCE_DEPTH: f32 = 0.18;

/// Lacrima's `AudioPlayer.RepeatMode` cycle (ToggleRepeat:
/// Off -> All -> One -> Off; Shuffle is set elsewhere).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// Prev/Next button intents — the caller resolves them against the
/// loaded cover items (MusicViewModel's Previous/NextMedia commands).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    None,
    Prev,
    Next,
}

#[derive(Default)]
pub struct Transport {
    pub album_list_open: bool,
    /// Where the band's search magnifier sits — the album-filter pill
    /// (drawn by the caller) anchors its collapsed icon here. It's a
    /// bigger expanding search box, not a button.
    pub search_icon: Pos2,
    pub repeat: RepeatMode,
    /// 0 carousel, 1 vertical grid, 2 horizontal grid — Lacrima's
    /// CoverLayoutMode toggle.
    pub cover_layout: u8,
    pub equalizer_open: bool,
    /// Press-down instants driving the deflate/inflate bounce, per id.
    bounce: HashMap<egui::Id, Instant>,
    /// Whether each button was down last frame (press-edge detect).
    pressed: HashMap<egui::Id, bool>,
    /// Volume knob relative-drag anchor: (volume, pointer angle deg)
    /// at grab time, so turning can't jump across the dead zone.
    vol_drag: Option<(f32, f32)>,
    /// Whether the current press started on the knob's cap — only
    /// those clicks toggle mute (ring presses just turn it).
    cap_press: bool,
    /// Where the cap press began — a release near it counts as a
    /// click even when egui's tiny drag threshold already tripped.
    cap_origin: Option<Pos2>,
    /// Total pointer rotation accumulated during the current press —
    /// a click requires ~zero travel; any real turn disqualifies it
    /// even if the release lands back at the grab point.
    drag_travel: f32,
    /// Cap currently held down — drives the squash animation.
    cap_down: bool,
    /// Volume captured when muted by a cap click — restored on the
    /// next click. `Some` doubles as the muted flag for drawing.
    muted_vol: Option<f32>,
    /// Full<->compact crossfade position: 0 = full row, 1 = compact
    /// strip (kebab + prev/play/next + knob).
    compact_t: f32,
    /// Overflow drawer open — the compact strip's kebab toggles it;
    /// the app-level Esc chain or leaving compact mode closes it.
    pub overflow_open: bool,
    /// Ctrl+F opened the drawer — focus the filter field once the
    /// reveal lands.
    want_filter_focus: bool,
    /// The drawer filter pill's collapsed/open state — a right-docked
    /// circle that grows leftward on hover, like the top-right
    /// SearchPill (`expand: hover || focus || has text`).
    filter_expanded: bool,
    filter_focused: bool,
    /// Play pressed while the player couldn't toggle (no player yet,
    /// or Stopped/Finished — `toggle()` only maps Playing/Paused).
    /// The app consumes it via `take_play_request` and picks what to
    /// start instead.
    play_request: bool,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            album_list_open: true,
            ..Default::default()
        }
    }

    /// The compact strip is (or is becoming) the dominant layout —
    /// callers gate the band search pill on this and route its
    /// Ctrl+F into the overflow menu.
    pub fn compact(&self) -> bool {
        self.compact_t > 0.5
    }

    /// Raw crossfade position (0 = full, 1 = compact) — sibling
    /// elements (the centered overlay text, the top info bar) ride
    /// the same fade.
    pub fn compact_mix(&self) -> f32 {
        self.compact_t
    }

    /// Surface a play intent the band can't serve itself — used by the
    /// other play surfaces (turntable arm, media key) so every "play"
    /// lands on the same fallback ladder.
    pub fn request_play(&mut self) {
        self.play_request = true;
    }

    /// A play click arrived while nothing could toggle — the app
    /// decides what to start (selection → loaded album → first album).
    pub fn take_play_request(&mut self) -> bool {
        std::mem::take(&mut self.play_request)
    }

    /// Ctrl+F into the album filter — opens the overflow drawer and
    /// focuses its field once it exists.
    pub fn open_filter(&mut self) {
        self.overflow_open = true;
        self.want_filter_focus = true;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        accent: Color32,
        s: f32,
    ) -> Step {
        let mut step = Step::None;
        // Below the edge-to-edge span the three clusters collide —
        // crossfade the full row into the compact strip.
        let compact = compact_needed(rect.width(), s);
        let ct = ui
            .ctx()
            .animate_bool_with_time(ui.id().with("t-compact"), compact, COMPACT_SECS);
        self.compact_t = ct;
        if !compact {
            self.overflow_open = false;
        }
        if ct < 0.999 {
            step = self.row_full(
                ui,
                rect,
                player,
                alpha * (1.0 - ct),
                interactive && ct < 0.5,
                accent,
                s,
            );
        }
        if ct > 0.001 {
            let st = self.row_compact(
                ui,
                rect,
                player,
                alpha * ct,
                interactive && ct >= 0.5,
                accent,
                s,
            );
            if step == Step::None {
                step = st;
            }
        }
        step
    }

    /// Lacrima's full-width layout: album-list + search slot at the
    /// left, stop/prev/play/next/repeat centered, layout + eq +
    /// volume at the right.
    #[allow(clippy::too_many_arguments)]
    fn row_full(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        accent: Color32,
        s: f32,
    ) -> Step {
        let mut step = Step::None;
        let cy = rect.center().y;
        let hit = |r: Rect, id: &str, tip: &str| {
            if interactive {
                ui.interact(r, ui.id().with(id), Sense::click())
            } else {
                ui.interact(r, ui.id().with(id), Sense::hover())
            }
            .on_hover_text(tip)
        };

        // --- Left: album-list toggle (Margin 50,0,0,0) ----------------
        let left = Rect::from_center_size(
            Pos2::new(rect.min.x + SIDE_MARGIN * s + BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );
        let r = hit(left, "t-list", "Album List");
        if r.clicked() {
            self.album_list_open = !self.album_list_open;
        }
        hover_cursor(ui, &r);
        let ic = scaled(left, self.press_scale(ui, &r));
        icon_album_list(ui.painter(), ic, self.album_list_open, alpha);

        // Search slot to its right (same left StackPanel, Spacing=50) —
        // not a button: the album-filter pill lives here, expanding
        // rightward on hover. The caller draws it at `search_icon`.
        let search = Rect::from_center_size(
            Pos2::new(left.max.x + RIGHT_SPACING * s + BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );
        // Nudge down a couple px: the toggle's glyph mass (top bar +
        // square) centers slightly below the slot center, and the
        // magnifier reads high beside it otherwise.
        self.search_icon = Pos2::new(search.center().x, cy + 1.5 * s);

        // --- Center: stop / prev / play / next / repeat ---------------
        let widths = [BTN, BTN, PLAY_BTN, BTN, BTN];
        let total = widths.iter().sum::<f32>() * s + CENTER_SPACING * s * 4.0;
        let mut x = rect.center().x - total * 0.5;
        let at = |i: usize, x: &mut f32| {
            let w = widths[i] * s;
            let r = Rect::from_min_size(Pos2::new(*x, cy - w * 0.5), Vec2::splat(w));
            *x += w + CENTER_SPACING * s;
            r
        };

        let stop = at(0, &mut x);
        let prev = at(1, &mut x);
        let play = at(2, &mut x);
        let next = at(3, &mut x);
        let repeat = at(4, &mut x);

        let playing = matches!(player.map(|p| p.state()), Some(PlayerState::Playing));

        let r = hit(stop, "t-stop", "Stop");
        if r.clicked() {
            if let Some(p) = player {
                p.stop();
            }
        }
        hover_cursor(ui, &r);
        let ic = scaled(stop, self.press_scale(ui, &r));
        icon_stop(ui.painter(), ic, alpha);

        let r = hit(prev, "t-prev", "Previous Track");
        if r.clicked() {
            step = Step::Prev;
        }
        hover_cursor(ui, &r);
        let ic = scaled(prev, self.press_scale(ui, &r));
        icon_prev(ui.painter(), ic, alpha);

        let r = hit(play, "t-play", "Play / Pause");
        if r.clicked() {
            match player {
                // Playing/Paused toggle in place — anything else (no
                // player yet, Stopped, Finished) asks the app to
                // start something (Lacrima's TogglePlay falls back to
                // the selected item).
                Some(p)
                    if matches!(
                        p.state(),
                        PlayerState::Playing | PlayerState::Paused
                    ) =>
                {
                    p.toggle()
                }
                _ => self.play_request = true,
            }
        }
        hover_cursor(ui, &r);
        let ic = scaled(play, self.press_scale(ui, &r));
        icon_play(ui.painter(), ic, playing, alpha);

        let r = hit(next, "t-next", "Next Track");
        if r.clicked() {
            step = Step::Next;
        }
        hover_cursor(ui, &r);
        let ic = scaled(next, self.press_scale(ui, &r));
        icon_next(ui.painter(), ic, alpha);

        let r = hit(repeat, "t-repeat", "Repeat: Off / All / One");
        if r.clicked() {
            self.repeat = match self.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            };
        }
        hover_cursor(ui, &r);
        let ic = scaled(repeat, self.press_scale(ui, &r));
        icon_repeat(ui.painter(), ic, self.repeat, alpha);

        // --- Right: cover layout / equalizer / volume -----------------
        let vol_w = KNOB_D * s;
        let vol_rect = Rect::from_min_size(
            Pos2::new(
                rect.max.x - SIDE_MARGIN * s - vol_w,
                cy - KNOB_D * 0.5 * s,
            ),
            Vec2::new(vol_w, KNOB_D * s),
        );
        let eq = Rect::from_center_size(
            Pos2::new(vol_rect.min.x - RIGHT_SPACING * s - BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );
        let layout = Rect::from_center_size(
            Pos2::new(eq.min.x - RIGHT_SPACING * s - BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );

        let r = hit(layout, "t-layout", "Cover Layout");
        if r.clicked() {
            self.cover_layout = (self.cover_layout + 1) % 3;
        }
        hover_cursor(ui, &r);
        let ic = scaled(layout, self.press_scale(ui, &r));
        icon_layout(ui.painter(), ic, self.cover_layout, alpha);

        let r = hit(eq, "t-eq", "Equalizer");
        if r.clicked() {
            self.equalizer_open = !self.equalizer_open;
        }
        hover_cursor(ui, &r);
        let ic = scaled(eq, self.press_scale(ui, &r));
        icon_eq(ui.painter(), ic, self.equalizer_open, alpha);

        self.volume(ui, vol_rect, player, alpha, interactive, accent, s, "t-vol");
        step
    }

    /// Compact strip for narrow windows: kebab (overflow drawer) at
    /// the left, prev/play/next centered, volume at the right — the
    /// rest of the band's controls live in the kebab's reveal drawer
    /// (`overflow_strip`, drawn by the caller under the band).
    #[allow(clippy::too_many_arguments)]
    fn row_compact(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        accent: Color32,
        s: f32,
    ) -> Step {
        let mut step = Step::None;
        let cy = rect.center().y;
        // Ultra-narrow fallback: shrink the strip itself rather than
        // spill over the edges.
        let need = (C_PAD * 2.0
            + BTN * 2.0
            + BTN * 2.0
            + PLAY_BTN
            + CENTER_SPACING * 3.0
            + KNOB_D)
            * s;
        let cs = s * (rect.width() / need).min(1.0);
        let hit = |r: Rect, id: &str, tip: &str| {
            if interactive {
                ui.interact(r, ui.id().with(id), Sense::click())
            } else {
                ui.interact(r, ui.id().with(id), Sense::hover())
            }
            .on_hover_text(tip)
        };

        // --- Left: kebab -> overflow panel --------------------------
        let kebab = Rect::from_center_size(
            Pos2::new(rect.min.x + C_PAD * cs + BTN * 0.5 * cs, cy),
            Vec2::splat(BTN * cs),
        );
        let r = hit(kebab, "t-more", "More");
        if r.clicked() {
            self.overflow_open = !self.overflow_open;
        }
        hover_cursor(ui, &r);
        let ic = scaled(kebab, self.press_scale(ui, &r));
        icon_more(ui.painter(), ic, alpha);

        // Album-list toggle docks right of the kebab at the
        // center-cluster spacing.
        let list = Rect::from_center_size(
            Pos2::new(kebab.max.x + CENTER_SPACING * cs + BTN * 0.5 * cs, cy),
            Vec2::splat(BTN * cs),
        );
        let r = hit(list, "t-clist", "Album List");
        if r.clicked() {
            self.album_list_open = !self.album_list_open;
        }
        hover_cursor(ui, &r);
        let ic = scaled(list, self.press_scale(ui, &r));
        icon_album_list(ui.painter(), ic, self.album_list_open, alpha);

        // --- Center: prev / play / next -----------------------------
        let widths = [BTN, PLAY_BTN, BTN];
        let total = widths.iter().sum::<f32>() * cs + CENTER_SPACING * cs * 2.0;
        let mut x = rect.center().x - total * 0.5;
        let at = |i: usize, x: &mut f32| {
            let w = widths[i] * cs;
            let r = Rect::from_min_size(Pos2::new(*x, cy - w * 0.5), Vec2::splat(w));
            *x += w + CENTER_SPACING * cs;
            r
        };
        let prev = at(0, &mut x);
        let play = at(1, &mut x);
        let next = at(2, &mut x);
        let playing = matches!(player.map(|p| p.state()), Some(PlayerState::Playing));

        // `c`-prefixed ids: during the crossfade both rows are drawn
        // and two interacts under one id would alias.
        let r = hit(prev, "t-cprev", "Previous Track");
        if r.clicked() {
            step = Step::Prev;
        }
        hover_cursor(ui, &r);
        let ic = scaled(prev, self.press_scale(ui, &r));
        icon_prev(ui.painter(), ic, alpha);

        let r = hit(play, "t-cplay", "Play / Pause");
        if r.clicked() {
            match player {
                Some(p)
                    if matches!(
                        p.state(),
                        PlayerState::Playing | PlayerState::Paused
                    ) =>
                {
                    p.toggle()
                }
                _ => self.play_request = true,
            }
        }
        hover_cursor(ui, &r);
        let ic = scaled(play, self.press_scale(ui, &r));
        icon_play(ui.painter(), ic, playing, alpha);

        let r = hit(next, "t-cnext", "Next Track");
        if r.clicked() {
            step = Step::Next;
        }
        hover_cursor(ui, &r);
        let ic = scaled(next, self.press_scale(ui, &r));
        icon_next(ui.painter(), ic, alpha);

        // --- Right: volume -------------------------------------------
        let vol_rect = Rect::from_min_size(
            Pos2::new(rect.max.x - C_PAD * cs - KNOB_D * cs, cy - KNOB_D * 0.5 * cs),
            Vec2::new(KNOB_D * cs, KNOB_D * cs),
        );
        self.volume(ui, vol_rect, player, alpha, interactive, accent, cs, "t-cvol");
        step
    }

    /// The kebab's reveal drawer (compact strip): the full row's
    /// hidden controls laid out horizontally — the album filter
    /// field, stop, repeat, layout, eq — same glyphs and
    /// tooltips as the full band. `rect` is the FULL-height strip;
    /// `clip` is the revealed window — the caller slides `rect` down
    /// through `clip`'s bottom edge as the drawer closes, so the
    /// icons translate at fixed size instead of being squashed.
    #[allow(clippy::too_many_arguments)]
    pub fn overflow_strip(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        clip: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        s: f32,
        search_text: &mut String,
    ) {
        let cy = rect.center().y;
        // `ui` is passed per-call — capturing it here would hold the
        // borrow across `scope_builder` below. Controls only sense on
        // their visible part (intersected with `clip`).
        let hit = |ui: &egui::Ui, r: Rect, id: &str, tip: &str| {
            let r = r.intersect(clip);
            if interactive {
                ui.interact(r, ui.id().with(id), Sense::click())
            } else {
                ui.interact(r, ui.id().with(id), Sense::hover())
            }
            .on_hover_text(tip)
        };
        // Painted content is clipped to the revealed window — the
        // strip slides through it at full height.
        let cp = ui.painter().with_clip_rect(clip);

        // Buttons left-aligned so the first one sits vertically under
        // the kebab (same C_PAD inset); the filter field right-aligns
        // at the same margin — the album-list toggle lives on the
        // compact strip itself.
        let sp = RIGHT_SPACING * s;
        let mut x = rect.min.x + C_PAD * s;
        let btn_at = |x: &mut f32| {
            let r = Rect::from_center_size(
                Pos2::new(*x + BTN * 0.5 * s, cy),
                Vec2::splat(BTN * s),
            );
            *x += BTN * s + sp;
            r
        };
        let stop = btn_at(&mut x);
        let repeat = btn_at(&mut x);
        let layout = btn_at(&mut x);
        let eq = btn_at(&mut x);
        // Album filter pill — a right-docked magnifier circle that
        // grows LEFTWARD on hover/focus like the top-right SearchPill
        // (PILL spec behavior, BAND metrics), bounded left by the
        // buttons row. `x` still points just past the last button.
        let pill_id = ui.id().with("ovf-filter");
        let open_t = ease_out_cubic(ui.ctx().animate_bool_with_time(
            pill_id.with("open"),
            self.filter_expanded,
            0.35,
        ));
        let pill_right = rect.max.x - C_PAD * s;
        let pill_w = ((50.0 + 250.0 * open_t) * s).min((pill_right - x).max(50.0 * s));
        let pill_h = 50.0 * s;
        let field = Rect::from_min_max(
            Pos2::new(pill_right - pill_w, cy - pill_h * 0.5),
            Pos2::new(pill_right, cy + pill_h * 0.5),
        );
        // The pointer test runs on the pre-growth rect — the inner
        // TextEdit claims hover for itself once open (SearchPill's
        // track_expand). Open while hovered / focused / Ctrl+F
        // pending / holding text.
        let pill_hover = ui
            .ctx()
            .input(|i| i.pointer.latest_pos())
            .is_some_and(|p| field.intersect(clip).contains(p));
        self.filter_expanded = interactive
            && (pill_hover
                || self.filter_focused
                || self.want_filter_focus
                || !search_text.is_empty());
        if pill_hover && !self.filter_expanded {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let rr = CornerRadius::same((pill_h * 0.5).round() as u8);
        cp.rect_filled(
            field,
            rr,
            Color32::from_black_alpha((140.0 * open_t * alpha).min(255.0) as u8),
        );
        // Collapsed face sits dimmed — brightening as the box opens
        // (SearchPill's grow_left border treatment).
        let dim = 0.5 + 0.5 * open_t;
        cp.rect_stroke(
            field,
            rr,
            Stroke::new(
                (2.5 - 1.5 * open_t) * s,
                Color32::from_white_alpha(
                    ((0xB0 as f32 - (0xB0 - 0x44) as f32 * open_t) * alpha * dim) as u8,
                ),
            ),
            StrokeKind::Inside,
        );
        // The magnifier rides the box's left edge, travelling left as
        // the pill opens — same anchoring as the top-right pill.
        magnifier_icon(
            &cp,
            Pos2::new(field.min.x + 25.0 * s, cy),
            25.0 * s / 24.0,
            Color32::from_white_alpha((0xCC as f32 * alpha * dim) as u8),
        );
        if open_t > 0.05 {
            // The scope rect itself is text-height and centered on
            // the strip (the search_pill pattern) — a taller rect
            // relies on cross-axis centering TextEdit doesn't honor.
            let line_h = 15.0 * s * 1.4;
            let clear = !search_text.is_empty();
            let edit_rect = Rect::from_min_max(
                Pos2::new(field.min.x + 49.0 * s, cy - line_h * 0.5),
                Pos2::new(
                    field.max.x - if clear { 30.0 } else { 8.0 } * s,
                    cy + line_h * 0.5,
                ),
            );
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(edit_rect.intersect(clip)),
                |ui| {
                    ui.style_mut()
                        .text_styles
                        .insert(egui::TextStyle::Body, FontId::proportional(15.0 * s));
                    let edit = ui.add(
                        egui::TextEdit::singleline(search_text)
                            .id(pill_id.with("edit"))
                            .desired_width(f32::INFINITY)
                            .font(FontId::proportional(15.0 * s))
                            .text_color(Color32::WHITE.gamma_multiply(alpha))
                            .hint_text("Filter albums")
                            .interactive(interactive)
                            .frame(egui::Frame::NONE),
                    );
                    if self.want_filter_focus {
                        ui.ctx().memory_mut(|m| m.request_focus(edit.id));
                        self.want_filter_focus = false;
                    }
                    self.filter_focused = edit.has_focus();
                },
            );
            if clear && open_t > 0.6 {
                let cr = Rect::from_center_size(
                    Pos2::new(field.max.x - 15.0 * s, cy),
                    Vec2::splat(16.0 * s),
                );
                let c = Color32::from_white_alpha((0xAA as f32 * alpha) as u8);
                let g = 3.5 * s;
                cp.line_segment(
                    [cr.min + Vec2::splat(g), cr.max - Vec2::splat(g)],
                    Stroke::new(1.6 * s, c),
                );
                cp.line_segment(
                    [
                        Pos2::new(cr.max.x - g, cr.min.y + g),
                        Pos2::new(cr.min.x + g, cr.max.y - g),
                    ],
                    Stroke::new(1.6 * s, c),
                );
                let clr = hit(ui, cr, "t-ofilter-clr", "Clear");
                if clr.clicked() {
                    search_text.clear();
                }
            }
        } else {
            self.filter_focused = false;
        }

        let r = hit(ui, stop, "t-ostop", "Stop");
        if r.clicked() {
            if let Some(p) = player {
                p.stop();
            }
        }
        hover_cursor(ui, &r);
        let ic = scaled(stop, self.press_scale(ui, &r));
        icon_stop(&cp, ic, alpha);

        let r = hit(ui, repeat, "t-orepeat", "Repeat: Off / All / One");
        if r.clicked() {
            self.repeat = match self.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            };
        }
        hover_cursor(ui, &r);
        let ic = scaled(repeat, self.press_scale(ui, &r));
        icon_repeat(&cp, ic, self.repeat, alpha);

        let r = hit(ui, layout, "t-olayout", "Cover Layout");
        if r.clicked() {
            self.cover_layout = (self.cover_layout + 1) % 3;
        }
        hover_cursor(ui, &r);
        let ic = scaled(layout, self.press_scale(ui, &r));
        icon_layout(&cp, ic, self.cover_layout, alpha);

        let r = hit(ui, eq, "t-oeq", "Equalizer");
        if r.clicked() {
            self.equalizer_open = !self.equalizer_open;
        }
        hover_cursor(ui, &r);
        let ic = scaled(eq, self.press_scale(ui, &r));
        icon_eq(ui.painter(), ic, self.equalizer_open, alpha);
    }

    /// Deflate/inflate press feedback: on the press-down edge record a
    /// bounce start; the returned scale follows a sine lobe back to 1.0
    /// so even a fast click completes the whole squash smoothly.
    fn press_scale(&mut self, ui: &egui::Ui, resp: &egui::Response) -> f32 {
        let now = Instant::now();
        let down = resp.is_pointer_button_down_on();
        let was = self.pressed.insert(resp.id, down).unwrap_or(false);
        if down && !was {
            self.bounce.insert(resp.id, now);
        }
        let mut any = false;
        self.bounce.retain(|_, t0| {
            let u = now.duration_since(*t0).as_secs_f32() / BOUNCE_SECS;
            any |= u < 1.0;
            u < 1.0
        });
        if any {
            ui.ctx().request_repaint();
        }
        match self.bounce.get(&resp.id) {
            Some(t0) => {
                let u = (now.duration_since(*t0).as_secs_f32() / BOUNCE_SECS).min(1.0);
                1.0 - BOUNCE_DEPTH * (std::f32::consts::PI * u).sin()
            }
            None => 1.0,
        }
    }

    /// Rotary volume knob: dark ring with a cyan level arc sweeping
    /// 270° (dead zone at the bottom), a conical metal cap built as a
    /// vertex-shaded fan, and a cyan pip marking the position. Drag
    /// rotates to the pointer angle; wheel nudges by 5%. `id` is the
    /// interact salt — the two layouts must not share one during the
    /// crossfade.
    #[allow(clippy::too_many_arguments)]
    fn volume(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        accent: Color32,
        s: f32,
        id: &str,
    ) {
        // Slow breathing pulse on the level arc — ~4.5s cycle. The
        // core stays near-full while the halo swings wide, so the
        // neon visibly blooms and settles.
        let wave = (ui.ctx().input(|i| i.time) as f32 * 1.4).sin();
        ui.ctx().request_repaint();
        let pulse = 0.72 + 0.28 * wave;
        let accent = accent.gamma_multiply(pulse);
        let c = rect.center();
        let r = rect.height() * 0.5;
        let cap_r = r * 0.66;
        let mut vol = player.map(|p| p.volume()).unwrap_or(0.8).clamp(0.0, 1.0);
        if interactive {
            let resp = ui
                .interact(rect, ui.id().with(id), Sense::click_and_drag())
                .on_hover_text("Volume — drag to adjust, click the cap to mute");
            // Cap press/release edges handled manually: egui's
            // clicked() dies the moment drag_started fires (a couple
            // px of jitter), which turns most cap clicks into tiny
            // rotations. A release within 8px of the press point is a
            // click — toggles mute; further than that was a turn.
            let down = resp.is_pointer_button_down_on();
            let was = self.pressed.insert(resp.id, down).unwrap_or(false);
            let pos = resp
                .interact_pointer_pos()
                .or_else(|| ui.ctx().input(|i| i.pointer.interact_pos()));
            if down && !was {
                self.cap_press = pos.is_some_and(|p| p.distance(c) <= cap_r);
                self.cap_origin = pos;
                self.drag_travel = 0.0;
            }
            self.cap_down = down && self.cap_press;
            // Release edge: a click needs ~zero pointer travel AND
            // ~zero accumulated rotation — a turn that lands back
            // near the grab point is still a turn, not a mute.
            if !down && was {
                let moved = pos
                    .and_then(|p| self.cap_origin.map(|o| p.distance(o)))
                    .unwrap_or(f32::MAX);
                let clicked = moved < 8.0 * s && self.drag_travel < 15.0;
                if clicked && self.cap_press {
                    if let Some(prev) = self.muted_vol.take() {
                        vol = prev;
                    } else {
                        self.muted_vol = Some(vol.max(0.05));
                        vol = 0.0;
                    }
                    if let Some(p) = player {
                        p.set_volume(vol);
                    }
                } else if clicked {
                    // Ring click jumps the level to that angle
                    // (dead zone clamps to the nearer end).
                    if let Some(p0) = self.cap_origin {
                        let a = (p0.y - c.y)
                            .atan2(p0.x - c.x)
                            .to_degrees()
                            .rem_euclid(360.0);
                        vol = if (45.0..KNOB_START).contains(&a) {
                            if a < 90.0 { 1.0 } else { 0.0 }
                        } else {
                            ((if a < 45.0 { a + 360.0 } else { a }) - KNOB_START)
                                .clamp(0.0, KNOB_SWEEP)
                                / KNOB_SWEEP
                        };
                        self.muted_vol = None;
                        if let Some(p) = player {
                            p.set_volume(vol);
                        }
                    }
                }
                self.cap_press = false;
                self.cap_origin = None;
            }
            // Relative rotary drag: the value accumulates per-frame
            // angular deltas, so sweeping through the bottom dead
            // zone just saturates at the nearer end — it can never
            // jump max->min (or back).
            if let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                let a = (p.y - c.y).atan2(p.x - c.x).to_degrees().rem_euclid(360.0);
                if resp.drag_started() {
                    self.vol_drag = Some((vol, a));
                }
                if down {
                    if let Some((v0, a0)) = self.vol_drag {
                        let da = (a - a0 + 540.0).rem_euclid(360.0) - 180.0;
                        vol = (v0 + da / KNOB_SWEEP).clamp(0.0, 1.0);
                        self.vol_drag = Some((vol, a));
                        self.drag_travel += da.abs();
                        self.muted_vol = None;
                    }
                }
            }
            if !down {
                self.vol_drag = None;
                self.cap_press = false;
                self.cap_down = false;
            }
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if resp.hovered() && scroll != 0.0 {
                vol = (vol + scroll.signum() * 0.05).clamp(0.0, 1.0);
                self.muted_vol = None;
            }
            if let (Some(p), true) = (player, down || scroll != 0.0) {
                p.set_volume(vol);
            }
            hover_cursor(ui, &resp);
        } else {
            self.vol_drag = None;
            self.cap_down = false;
        }

        // Muted: the accent eases out to gray and the residual arc
        // drains from the captured level — a visible power-down, not
        // a snap. `draw_vol` renders the drain; `vol` stays 0 for
        // logic while muted.
        let muted = self.muted_vol.is_some();
        let mute_t = ui
            .ctx()
            .animate_bool_with_time(ui.id().with("t-vol-mute"), muted, 0.3);
        let accent = {
            let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * mute_t) as u8;
            Color32::from_rgb(
                l(accent.r(), DIM.r()),
                l(accent.g(), DIM.g()),
                l(accent.b(), DIM.b()),
            )
        };
        let draw_vol = if muted {
            self.muted_vol.unwrap_or(0.0) * (1.0 - mute_t)
        } else {
            vol
        };
        // Cap squash while held — deflates ~10% with a quick ease.
        let cap_t = ui
            .ctx()
            .animate_bool_with_time(ui.id().with("t-vol-cap"), self.cap_down, 0.12);
        let cap_r = cap_r * (1.0 - 0.10 * cap_t);
        let painter = ui.painter();
        let arc_r = r * 0.845;
        let arc_w = 4.0 * s;
        let arc = |a0: f32, a1: f32, radius: f32| -> Vec<Pos2> {
            let n = ((a1 - a0).abs() / 4.0).ceil().max(1.0) as usize;
            (0..=n)
                .map(|i| {
                    let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
                    Pos2::new(c.x + radius * a.cos(), c.y + radius * a.sin())
                })
                .collect()
        };

        // Ring body + rim highlight.
        painter.circle_filled(c, r, KNOB_RING.gamma_multiply(alpha));
        painter.circle_stroke(
            c,
            r - 0.8 * s,
            Stroke::new(1.6 * s, KNOB_RING_EDGE.gamma_multiply(alpha)),
        );

        // Level arc: full track, then the value over it with a soft
        // halo underneath (fake bloom for the cyan).
        painter.add(Shape::line(
            arc(KNOB_START, KNOB_START + KNOB_SWEEP, arc_r),
            Stroke::new(arc_w, KNOB_TRACK.gamma_multiply(alpha)),
        ));
        if draw_vol > 0.001 {
            let pts = arc(KNOB_START, KNOB_START + draw_vol * KNOB_SWEEP, arc_r);
            // Neon glow: stacked halo rings widening outward, each
            // fainter than the last, under the solid core line. The
            // halo alpha rides the pulse squared — it blooms twice as
            // hard as the core dims.
            let glow = pulse * pulse;
            for (wk, ak) in [(6.0f32, 0.10), (3.6, 0.22), (2.0, 0.40)] {
                painter.add(Shape::line(
                    pts.clone(),
                    Stroke::new(arc_w * wk, accent.gamma_multiply(alpha * ak * glow)),
                ));
            }
            painter.add(Shape::line(
                pts,
                Stroke::new(arc_w, accent.gamma_multiply(alpha)),
            ));
        }

        // Conical metal cap: triangle fan whose rim brightness follows
        // a cosine lobe peaking straight up — the brushed-knob look.
        let n = 64;
        let light = 270.0f32.to_radians();
        // The cap dims toward gray while the mute fade runs.
        let dim = 1.0 - 0.45 * mute_t;
        let mut mesh = egui::epaint::Mesh::default();
        mesh.colored_vertex(
            c,
            Color32::from_gray((0xE8 as f32 * dim) as u8).gamma_multiply(alpha),
        );
        for i in 0..=n {
            let a = std::f32::consts::TAU * i as f32 / n as f32;
            let t = 0.5 + 0.5 * (a - light).cos();
            let g = ((96.0 + 150.0 * t.powf(1.2)) * dim) as u8;
            mesh.colored_vertex(
                Pos2::new(c.x + cap_r * a.cos(), c.y + cap_r * a.sin()),
                Color32::from_gray(g).gamma_multiply(alpha),
            );
        }
        for i in 1..=n {
            mesh.indices.extend_from_slice(&[0, i as u32, i as u32 + 1]);
        }
        painter.add(Shape::mesh(mesh));
        painter.circle_stroke(
            c,
            cap_r,
            Stroke::new(
                1.4 * s,
                Color32::from_black_alpha((140.0 * alpha).min(255.0) as u8),
            ),
        );

        // Position pip on the cap's outer edge.
        let pa = (KNOB_START + draw_vol * KNOB_SWEEP).to_radians();
        let pp = Pos2::new(
            c.x + cap_r * 0.84 * pa.cos(),
            c.y + cap_r * 0.84 * pa.sin(),
        );
        painter.circle_filled(pp, 3.8 * s, accent.gamma_multiply(alpha));
    }
}

fn scaled(rect: Rect, k: f32) -> Rect {
    Rect::from_center_size(rect.center(), rect.size() * k)
}

fn hover_cursor(ui: &egui::Ui, r: &egui::Response) {
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// Map 100x100 (or `canvas`-sized) canvas coords onto the button rect —
/// Avalonia's Viewbox Stretch=Uniform with a centered square canvas.
fn cv(rect: Rect, canvas: f32, x: f32, y: f32) -> Pos2 {
    let k = rect.height() / canvas;
    let ox = rect.center().x - canvas * 0.5 * k;
    Pos2::new(ox + x * k, rect.min.y + y * k)
}

fn cv_rect(rect: Rect, canvas: f32, x: f32, y: f32, w: f32, h: f32) -> Rect {
    let k = rect.height() / canvas;
    Rect::from_min_size(cv(rect, canvas, x, y), Vec2::new(w * k, h * k))
}

fn tri(painter: &egui::Painter, pts: [Pos2; 3], color: Color32) {
    painter.add(Shape::convex_polygon(pts.to_vec(), color, Stroke::NONE));
}

/// Sampled cubic bezier in canvas coords (for the repeat arrowheads'
/// rounded elbows).
fn cubic(rect: Rect, canvas: f32, p0: (f32, f32), c1: (f32, f32), c2: (f32, f32), p1: (f32, f32)) -> Vec<Pos2> {
    (0..=8)
        .map(|i| {
            let t = i as f32 / 8.0;
            let u = 1.0 - t;
            let x = u * u * u * p0.0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * p1.0;
            let y = u * u * u * p0.1 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * p1.1;
            cv(rect, canvas, x, y)
        })
        .collect()
}

// --- Icons (MusicView.axaml button canvases) ---------------------------

/// Rounded top bar + outlined rounded square; lit when the list is open.
/// Shared with the Emulation view's section-list toggle.
pub(crate) fn icon_album_list(painter: &egui::Painter, rect: Rect, open: bool, alpha: f32) {
    let c = (if open { LIT } else { DIM }).gamma_multiply(alpha);
    let k = rect.height() / 100.0;
    painter.rect_filled(
        cv_rect(rect, 100.0, 18.0, 15.0, 64.0, 8.0),
        CornerRadius::same((4.0 * k) as u8),
        c,
    );
    painter.rect_stroke(
        cv_rect(rect, 100.0, 10.0, 30.0, 80.0, 58.0),
        CornerRadius::same((12.0 * k) as u8),
        Stroke::new(6.0 * k, c),
        StrokeKind::Inside,
    );
}

pub(crate) fn icon_stop(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let k = rect.height() / 100.0;
    painter.rect_filled(
        cv_rect(rect, 100.0, 20.0, 20.0, 60.0, 60.0),
        CornerRadius::same((8.0 * k) as u8),
        WHITE.gamma_multiply(alpha),
    );
}

pub(crate) fn icon_prev(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let k = rect.height() / 100.0;
    let c = WHITE.gamma_multiply(alpha);
    painter.rect_filled(
        cv_rect(rect, 100.0, 10.0, 15.0, 12.0, 70.0),
        CornerRadius::same((6.0 * k) as u8),
        c,
    );
    tri(
        painter,
        [
            cv(rect, 100.0, 85.0, 15.0),
            cv(rect, 100.0, 35.0, 50.0),
            cv(rect, 100.0, 85.0, 85.0),
        ],
        c,
    );
}

/// Circle with pause bars (playing) / play triangle (paused) —
/// Lacrima's ToggleEmulatorPause button. Shared with the Emulation
/// band's session pause control.
pub(crate) fn icon_play(painter: &egui::Painter, rect: Rect, playing: bool, alpha: f32) {
    let k = rect.height() / 100.0;
    let c = WHITE.gamma_multiply(alpha);
    painter.circle_stroke(
        cv(rect, 100.0, 50.0, 50.0),
        46.0 * k,
        Stroke::new(7.0 * k, c),
    );
    if playing {
        painter.rect_filled(
            cv_rect(rect, 100.0, 35.0, 30.0, 10.0, 40.0),
            CornerRadius::same((4.0 * k) as u8),
            c,
        );
        painter.rect_filled(
            cv_rect(rect, 100.0, 55.0, 30.0, 10.0, 40.0),
            CornerRadius::same((4.0 * k) as u8),
            c,
        );
    } else {
        tri(
            painter,
            [
                cv(rect, 100.0, 38.0, 30.0),
                cv(rect, 100.0, 72.0, 50.0),
                cv(rect, 100.0, 38.0, 70.0),
            ],
            c,
        );
    }
}

pub(crate) fn icon_next(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let k = rect.height() / 100.0;
    let c = WHITE.gamma_multiply(alpha);
    tri(
        painter,
        [
            cv(rect, 100.0, 15.0, 15.0),
            cv(rect, 100.0, 65.0, 50.0),
            cv(rect, 100.0, 15.0, 85.0),
        ],
        c,
    );
    painter.rect_filled(
        cv_rect(rect, 100.0, 78.0, 15.0, 12.0, 70.0),
        CornerRadius::same((6.0 * k) as u8),
        c,
    );
}

/// Two counter-curving arrows on a 120x120 canvas; "1" for RepeatOne.
/// Kebab: three vertically stacked dots — the compact strip's
/// overflow-menu button. Lit ring while the panel is open.
fn icon_more(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let k = rect.height() / 100.0;
    for &y in &[22.0f32, 50.0, 78.0] {
        painter.circle_filled(
            cv(rect, 100.0, 50.0, y),
            7.5 * k,
            WHITE.gamma_multiply(alpha),
        );
    }
}

/// Small magnifier glyph — the overflow panel's filter-field icon
/// (`k` = size/26 scale).
fn magnifier_icon(painter: &egui::Painter, center: Pos2, k: f32, c: Color32) {
    painter.circle_stroke(
        center + Vec2::new(-2.0 * k, -2.0 * k),
        6.0 * k,
        Stroke::new(2.0 * k, c),
    );
    painter.line_segment(
        [
            center + Vec2::new(3.0 * k, 3.0 * k),
            center + Vec2::new(7.5 * k, 7.5 * k),
        ],
        Stroke::new(2.2 * k, c),
    );
}

pub(crate) fn icon_repeat(
    painter: &egui::Painter,
    rect: Rect,
    mode: RepeatMode,
    alpha: f32,
) {
    let c = (if mode == RepeatMode::Off { DIM } else { LIT }).gamma_multiply(alpha);
    let canvas = 120.0;
    let stroke = Stroke::new(11.0 * rect.height() / canvas, c);
    // Top arrow: (15,65)->(15,45) elbow->(105,30), head at (105,30).
    let mut top = vec![cv(rect, canvas, 15.0, 65.0), cv(rect, canvas, 15.0, 45.0)];
    top.extend(cubic(rect, canvas, (15.0, 45.0), (15.0, 30.0), (15.0, 30.0), (30.0, 30.0)));
    top.push(cv(rect, canvas, 105.0, 30.0));
    painter.add(Shape::line(top, stroke));
    painter.line_segment(
        [cv(rect, canvas, 105.0, 30.0), cv(rect, canvas, 88.0, 14.0)],
        stroke,
    );
    painter.line_segment(
        [cv(rect, canvas, 105.0, 30.0), cv(rect, canvas, 88.0, 46.0)],
        stroke,
    );
    // Bottom arrow: (105,55)->(105,75) elbow->(15,90), head at (15,90).
    let mut bot = vec![cv(rect, canvas, 105.0, 55.0), cv(rect, canvas, 105.0, 75.0)];
    bot.extend(cubic(rect, canvas, (105.0, 75.0), (105.0, 90.0), (105.0, 90.0), (90.0, 90.0)));
    bot.push(cv(rect, canvas, 15.0, 90.0));
    painter.add(Shape::line(bot, stroke));
    painter.line_segment(
        [cv(rect, canvas, 15.0, 90.0), cv(rect, canvas, 32.0, 74.0)],
        stroke,
    );
    painter.line_segment(
        [cv(rect, canvas, 15.0, 90.0), cv(rect, canvas, 32.0, 106.0)],
        stroke,
    );
    if mode == RepeatMode::One {
        let k = rect.height() / canvas;
        painter.text(
            Pos2::new(rect.center().x, rect.min.y + 58.0 * k),
            Align2::CENTER_CENTER,
            "1",
            FontId::proportional(32.0 * k),
            c,
        );
    }
}

/// Cover-layout toggle: carousel / vertical grid / horizontal grid.
/// Shared with the Emulation view's band (same global CoverLayoutMode).
pub(crate) fn icon_layout(painter: &egui::Painter, rect: Rect, mode: u8, alpha: f32) {
    let k = rect.height() / 100.0;
    let c = WHITE.gamma_multiply(alpha);
    match mode {
        0 => {
            for x in [1.0, 6.0, 90.0, 95.0] {
                painter.rect_filled(
                    cv_rect(rect, 100.0, x, 25.0, 4.0, 50.0),
                    CornerRadius::same((2.0 * k) as u8),
                    c,
                );
            }
            painter.rect_stroke(
                cv_rect(rect, 100.0, 12.0, 12.0, 76.0, 76.0),
                CornerRadius::same((12.0 * k) as u8),
                Stroke::new(7.5 * k, c),
                StrokeKind::Inside,
            );
        }
        1 => {
            let c = c.gamma_multiply(0.9);
            for (x, y) in [(12.0, 12.0), (52.0, 12.0), (12.0, 52.0), (52.0, 52.0)] {
                painter.rect_filled(
                    cv_rect(rect, 100.0, x, y, 36.0, 36.0),
                    CornerRadius::same((5.0 * k) as u8),
                    c,
                );
            }
        }
        _ => {
            let c = c.gamma_multiply(0.9);
            for x in [10.0, 38.0, 66.0] {
                painter.rect_filled(
                    cv_rect(rect, 100.0, x, 12.0, 24.0, 76.0),
                    CornerRadius::same((5.0 * k) as u8),
                    c,
                );
            }
        }
    }
}

/// Emulator-viewport toggle (Lacrima's ToggleEmulatorViewport):
/// a swap glyph — two counter-pointing horizontal arrows (game ⇄
/// list). Lit while the emulator presentation covers the item list,
/// dim when the user switched back.
pub(crate) fn icon_viewport(painter: &egui::Painter, rect: Rect, shown: bool, alpha: f32) {
    let c = (if shown { LIT } else { DIM }).gamma_multiply(alpha);
    let stroke = Stroke::new(7.5 * rect.height() / 100.0, c);
    let cpt = |x: f32, y: f32| cv(rect, 100.0, x, y);
    // Top arrow pointing right.
    painter.add(Shape::line(vec![cpt(20.0, 34.0), cpt(74.0, 34.0)], stroke));
    painter.add(Shape::line(
        vec![cpt(60.0, 20.0), cpt(74.0, 34.0), cpt(60.0, 48.0)],
        stroke,
    ));
    // Bottom arrow pointing left.
    painter.add(Shape::line(vec![cpt(80.0, 66.0), cpt(26.0, 66.0)], stroke));
    painter.add(Shape::line(
        vec![cpt(40.0, 80.0), cpt(26.0, 66.0), cpt(40.0, 52.0)],
        stroke,
    ));
}

/// Capture-settings glyph (Lacrima's ToggleRenderOptions button):
/// two horizontal slider tracks with filled knob dots at different
/// positions. Lit while the render-options panel is open.
pub(crate) fn icon_sliders(painter: &egui::Painter, rect: Rect, open: bool, alpha: f32) {
    let c = (if open { LIT } else { DIM }).gamma_multiply(alpha);
    let k = rect.height() / 100.0;
    let stroke = Stroke::new(6.0 * k, c);
    for (y, kx) in [(33.0, 34.0), (67.0, 66.0)] {
        painter.add(Shape::line(
            vec![cv(rect, 100.0, 14.0, y), cv(rect, 100.0, 86.0, y)],
            stroke,
        ));
        painter.circle_filled(cv(rect, 100.0, kx, y), 11.0 * k, c);
    }
}

/// Close-emulator power glyph (Lacrima's CloseEmulator button):
/// an arc with a gap at the top + the vertical power line.
pub(crate) fn icon_power(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let c = WHITE.gamma_multiply(alpha);
    let k = rect.height() / 100.0;
    let stroke = Stroke::new(9.0 * k, c);
    painter.line_segment(
        [cv(rect, 100.0, 50.0, 12.0), cv(rect, 100.0, 50.0, 45.0)],
        stroke,
    );
    // ~270° arc, gap centred at 12 o'clock.
    let center = cv(rect, 100.0, 50.0, 55.0);
    let r = 33.0 * k;
    let pts: Vec<Pos2> = (-40..=220)
        .step_by(6)
        .map(|deg| {
            let rad = (deg as f32).to_radians();
            center + Vec2::new(rad.cos() * r, rad.sin() * r)
        })
        .collect();
    painter.add(egui::Shape::line(pts, stroke));
}

/// Emulator-update glyph: a filled gear ringed by two circular
/// arrows — the classic sync mark, here meaning a pending upstream
/// update.
pub(crate) fn icon_update(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let c = WHITE.gamma_multiply(alpha);
    let k = rect.height() / 100.0;
    let center = cv(rect, 100.0, 50.0, 50.0);

    // Filled cog with a real hole — pair each toothed-boundary vertex
    // with a same-angle inner vertex and triangulate the ring between
    // them, so the hub is genuinely transparent.
    const TEETH: usize = 8;
    let step = std::f32::consts::TAU / TEETH as f32;
    let (root_r, tip_r, hole_r) = (15.0 * k, 21.0 * k, 8.0 * k);
    let (half_tooth, half_gap) = (step * 0.18, step * 0.40);
    let mut boundary: Vec<(f32, f32)> = Vec::with_capacity(TEETH * 4);
    for i in 0..TEETH {
        let a = i as f32 * step;
        for (r, da) in [
            (root_r, -half_gap),
            (tip_r, -half_tooth),
            (tip_r, half_tooth),
            (root_r, half_gap),
        ] {
            boundary.push((a + da, r));
        }
    }
    let n = boundary.len();
    let mut mesh = egui::Mesh::default();
    for &(ang, r) in &boundary {
        mesh.colored_vertex(center + Vec2::new(ang.cos() * r, ang.sin() * r), c);
        mesh.colored_vertex(
            center + Vec2::new(ang.cos() * hole_r, ang.sin() * hole_r),
            c,
        );
    }
    for i in 0..n {
        let j = (i + 1) % n;
        let (oi, ii) = (2 * i as u32, 2 * i as u32 + 1);
        let (oj, ij) = (2 * j as u32, 2 * j as u32 + 1);
        mesh.indices
            .extend_from_slice(&[oi, oj, ij, oi, ij, ii]);
    }
    painter.add(Shape::Mesh(std::sync::Arc::new(mesh)));

    // Sync ring — two ~140° arcs; each arrowhead sits ON the ring a
    // few degrees past its arc's end (gap, not a tangent runaway),
    // pointing along the direction of travel: upper-right clockwise,
    // lower-left counter. (0° = right, 90° = down.)
    let r = 37.0 * k;
    let stroke = Stroke::new(7.0 * k, c);
    for (start, end) in [(135.0f32, 275.0f32), (315.0f32, 455.0f32)] {
        let pts: Vec<Pos2> = ((start as i32)..=(end as i32))
            .step_by(8)
            .map(|deg| {
                let rad = (deg as f32).to_radians();
                center + Vec2::new(rad.cos() * r, rad.sin() * r)
            })
            .collect();
        painter.add(Shape::line(pts, stroke));
        let a = (end + 8.0).to_radians();
        let pos = center + Vec2::new(a.cos() * r, a.sin() * r);
        let dir = Vec2::new(a.sin(), -a.cos());
        let perp = Vec2::new(-dir.y, dir.x);
        painter.add(Shape::convex_polygon(
            vec![
                pos + dir * 10.0 * k,
                pos - dir * 6.0 * k + perp * 8.0 * k,
                pos - dir * 6.0 * k - perp * 8.0 * k,
            ],
            c,
            Stroke::NONE,
        ));
    }
}

/// Input-tunnel toggle (the emulator-control button): a classic
/// pointer-cursor arrow, lit while clicks/keys forward into the
/// captured emulator window.
pub(crate) fn icon_pointer(painter: &egui::Painter, rect: Rect, on: bool, alpha: f32) {
    let c = (if on { LIT } else { DIM }).gamma_multiply(alpha);
    let k = rect.height() / 100.0;
    // Cursor silhouette: tip at top-left, notched tail — the Win32
    // arrow shape scaled to the 100px box.
    let pts = [
        cv(rect, 100.0, 34.0, 14.0),
        cv(rect, 100.0, 34.0, 72.0),
        cv(rect, 100.0, 44.0, 62.0),
        cv(rect, 100.0, 52.0, 82.0),
        cv(rect, 100.0, 60.0, 77.0),
        cv(rect, 100.0, 52.0, 56.0),
        cv(rect, 100.0, 66.0, 56.0),
    ];
    painter.add(Shape::convex_polygon(pts.to_vec(), c, Stroke::NONE));
    painter.add(Shape::closed_line(
        pts.to_vec(),
        Stroke::new(3.0 * k, c),
    ));
}

/// Equalizer: rounded frame with a divider, 4 vertical slider slots
/// with chunky knobs. Lit while the EQ drawer is open.
fn icon_eq(painter: &egui::Painter, rect: Rect, open: bool, alpha: f32) {
    let k = rect.height() / 100.0;
    let c = (if open { LIT } else { WHITE }).gamma_multiply(alpha);
    painter.rect_stroke(
        cv_rect(rect, 100.0, 6.0, 12.0, 88.0, 76.0),
        CornerRadius::same((9.0 * k) as u8),
        Stroke::new(7.5 * k, c),
        StrokeKind::Inside,
    );
    painter.line_segment(
        [cv(rect, 100.0, 6.0, 71.0), cv(rect, 100.0, 94.0, 71.0)],
        Stroke::new(7.5 * k, c),
    );
    let slots = [
        (24.0, 38.0, 14.0),
        (41.0, 22.0, 18.0),
        (58.0, 42.0, 12.0),
        (75.0, 18.0, 24.0),
    ];
    for (x, ky, kh) in slots {
        painter.line_segment(
            [cv(rect, 100.0, x, 25.0), cv(rect, 100.0, x, 58.0)],
            Stroke::new(4.5 * k, c),
        );
        painter.rect_filled(
            cv_rect(rect, 100.0, x - 5.0, ky, 10.0, kh),
            CornerRadius::same((2.0 * k) as u8),
            c,
        );
    }
}

/// Below the edge-to-edge cluster span the full row collides — the
/// band switches to the compact strip.
fn compact_needed(width: f32, s: f32) -> bool {
    width < FULL_SPAN * s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_only_below_cluster_span() {
        assert!(!compact_needed(FULL_SPAN * 1.01, 1.0));
        assert!(compact_needed(FULL_SPAN * 0.99, 1.0));
        // The threshold is in design units — UI scale shifts it.
        assert!(!compact_needed(FULL_SPAN * 0.6, 0.5));
        assert!(compact_needed(FULL_SPAN * 0.4, 0.5));
        // The span is the real layout's collision boundary: at s=1
        // the centered cluster's right edge sits exactly where the
        // right cluster begins — one design px less and they
        // overlap, which is precisely when the transition must run.
        assert!(!compact_needed(1200.0, 1.0));
        assert!(compact_needed(1199.0, 1.0));
        assert_eq!(FULL_SPAN, 1200.0);
    }
}

/// CubicEaseOut (Lacrima's width transition) — the drawer filter
/// pill's open animation.
fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}
