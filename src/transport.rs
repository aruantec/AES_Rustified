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
const VOLUME_W: f32 = 52.0;
const VOLUME_H: f32 = 100.0;

const WHITE: Color32 = Color32::WHITE;
const LIT: Color32 = Color32::from_rgb(0xF0, 0xF0, 0xF0);
const DIM: Color32 = Color32::from_rgb(0x69, 0x69, 0x69);

// Volume pill (VerticalVolumeSliderControl).
const VOL_TRACK_W: f32 = 46.0;
const VOL_TRACK_R: f32 = 23.0;
const VOL_INNER_PAD: f32 = 3.0;
const VOL_ICON_ZONE: f32 = 36.0;
const VOL_OUTLINE: Color32 = Color32::from_rgb(70, 72, 80);
const VOL_EMPTY: Color32 = Color32::from_rgb(115, 117, 125);
const VOL_ICON: Color32 = Color32::from_rgb(80, 82, 90); // IconBrush a=200 applied at draw
const VOL_ICON_A: f32 = 200.0 / 255.0;

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
}

impl Transport {
    pub fn new() -> Self {
        Self {
            album_list_open: true,
            ..Default::default()
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) -> Step {
        let mut step = Step::None;
        let cy = rect.center().y;
        let hit = |r: Rect, id: &str| {
            if interactive {
                ui.interact(r, ui.id().with(id), Sense::click())
            } else {
                ui.interact(r, ui.id().with(id), Sense::hover())
            }
        };

        // --- Left: album-list toggle (Margin 50,0,0,0) ----------------
        let left = Rect::from_center_size(
            Pos2::new(rect.min.x + SIDE_MARGIN * s + BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );
        let r = hit(left, "t-list");
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
        // Nudge down a few px: the toggle's glyph mass (top bar +
        // square) centers slightly below the slot center, and the
        // magnifier reads high beside it otherwise.
        self.search_icon = Pos2::new(search.center().x, cy + 4.0 * s);

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

        let r = hit(stop, "t-stop");
        if r.clicked() {
            if let Some(p) = player {
                p.stop();
            }
        }
        hover_cursor(ui, &r);
        let ic = scaled(stop, self.press_scale(ui, &r));
        icon_stop(ui.painter(), ic, alpha);

        let r = hit(prev, "t-prev");
        if r.clicked() {
            step = Step::Prev;
        }
        hover_cursor(ui, &r);
        let ic = scaled(prev, self.press_scale(ui, &r));
        icon_prev(ui.painter(), ic, alpha);

        let r = hit(play, "t-play");
        if r.clicked() {
            if let Some(p) = player {
                p.toggle();
            }
        }
        hover_cursor(ui, &r);
        let ic = scaled(play, self.press_scale(ui, &r));
        icon_play(ui.painter(), ic, playing, alpha);

        let r = hit(next, "t-next");
        if r.clicked() {
            step = Step::Next;
        }
        hover_cursor(ui, &r);
        let ic = scaled(next, self.press_scale(ui, &r));
        icon_next(ui.painter(), ic, alpha);

        let r = hit(repeat, "t-repeat");
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
        let vol_w = VOLUME_W * s;
        let vol_rect = Rect::from_min_size(
            Pos2::new(
                rect.max.x - SIDE_MARGIN * s - vol_w,
                cy - VOLUME_H * 0.5 * s,
            ),
            Vec2::new(vol_w, VOLUME_H * s),
        );
        let eq = Rect::from_center_size(
            Pos2::new(vol_rect.min.x - RIGHT_SPACING * s - BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );
        let layout = Rect::from_center_size(
            Pos2::new(eq.min.x - RIGHT_SPACING * s - BTN * 0.5 * s, cy),
            Vec2::splat(BTN * s),
        );

        let r = hit(layout, "t-layout");
        if r.clicked() {
            self.cover_layout = (self.cover_layout + 1) % 3;
        }
        hover_cursor(ui, &r);
        let ic = scaled(layout, self.press_scale(ui, &r));
        icon_layout(ui.painter(), ic, self.cover_layout, alpha);

        let r = hit(eq, "t-eq");
        if r.clicked() {
            self.equalizer_open = !self.equalizer_open;
        }
        hover_cursor(ui, &r);
        let ic = scaled(eq, self.press_scale(ui, &r));
        icon_eq(ui.painter(), ic, self.equalizer_open, alpha);

        self.volume(ui, vol_rect, player, alpha, interactive, s);
        step
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

    /// VerticalVolumeSliderControl: 46px pill, gray empty portion over
    /// white fill rising from the bottom, speaker glyph at the base.
    /// Drag sets volume; wheel nudges by 5%.
    fn volume(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        player: Option<&AudioPlayer>,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) {
        let mut vol = player.map(|p| p.volume()).unwrap_or(0.8).clamp(0.0, 1.0);
        if interactive {
            let resp = ui.interact(rect, ui.id().with("t-vol"), Sense::click_and_drag());
            if resp.is_pointer_button_down_on() {
                if let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    vol = ((rect.max.y - p.y) / rect.height()).clamp(0.0, 1.0);
                }
            }
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if resp.hovered() && scroll != 0.0 {
                vol = (vol + scroll.signum() * 0.05).clamp(0.0, 1.0);
            }
            if let (Some(p), true) = (player, resp.is_pointer_button_down_on() || scroll != 0.0) {
                p.set_volume(vol);
            }
            hover_cursor(ui, &resp);
        }

        let painter = ui.painter();
        let track_w = VOL_TRACK_W * s;
        let pad = VOL_INNER_PAD * s;
        let track = Rect::from_min_size(
            Pos2::new(rect.center().x - track_w * 0.5, rect.min.y),
            Vec2::new(track_w, rect.height()),
        );
        let outer_r = CornerRadius::same((VOL_TRACK_R * s).round() as u8);
        painter.rect_stroke(
            track,
            outer_r,
            Stroke::new(3.0 * s, VOL_OUTLINE.gamma_multiply(alpha)),
            StrokeKind::Inside,
        );
        // Gray capsule is the track background; the white value fills
        // upward from the bottom. Lacrima clips a square fill to the
        // rounded track; egui only clips axis-aligned, so the fill is
        // built as a convex polygon — straight sides down to the cap
        // center, then the semicircular cap (or just the chord segment
        // of it when the fill is shallower than the cap, which keeps
        // low values curved instead of a squashed line).
        let inner = track.shrink(pad);
        let inner_r = (VOL_TRACK_R - VOL_INNER_PAD) * s;
        painter.rect_filled(
            inner,
            CornerRadius::same(inner_r.round() as u8),
            VOL_EMPTY.gamma_multiply(alpha),
        );
        let fill_top = inner.min.y + inner.height() * (1.0 - vol);
        if fill_top <= inner.min.y {
            // 100%: the fill is the whole capsule.
            painter.rect_filled(
                inner,
                CornerRadius::same(inner_r.round() as u8),
                WHITE.gamma_multiply(alpha),
            );
        } else if fill_top < inner.max.y {
            // The fill is capsule ∩ {y >= fill_top} built as a convex
            // polygon: a flat chord at the fill boundary (narrowed when
            // it cuts through the top cap), the straight sides, and
            // the sampled bottom-cap arc — or just the cap's chord
            // segment when the fill is shallower than the cap.
            let cx = inner.center().x;
            let top_cy = inner.min.y + inner_r;
            let bot_cy = inner.max.y - inner_r;
            const N: usize = 12;
            let mut pts = Vec::new();
            if fill_top > bot_cy {
                // Chord cut across the bottom cap: a0 = the arc angle
                // at which y == fill_top.
                let a0 = ((fill_top - bot_cy) / inner_r).clamp(0.0, 1.0).asin();
                for i in 0..=N {
                    let a = a0 + (std::f32::consts::PI - 2.0 * a0) * i as f32 / N as f32;
                    pts.push(Pos2::new(cx + inner_r * a.cos(), bot_cy + inner_r * a.sin()));
                }
            } else if fill_top <= top_cy {
                // Chord inside the top cap: boundary runs chord -> arc
                // down the right of the top cap -> side -> bottom cap
                // arc -> side -> arc up the left of the top cap.
                let a0 = ((top_cy - fill_top) / inner_r).clamp(0.0, 1.0).asin();
                pts.push(Pos2::new(cx + inner_r * a0.cos(), fill_top));
                for i in 0..=N {
                    let a = a0 * (1.0 - i as f32 / N as f32);
                    pts.push(Pos2::new(
                        cx + inner_r * a.cos(),
                        top_cy - inner_r * a.sin(),
                    ));
                }
                pts.push(Pos2::new(inner.max.x, bot_cy));
                for i in 0..=N {
                    let a = std::f32::consts::PI * i as f32 / N as f32;
                    pts.push(Pos2::new(cx + inner_r * a.cos(), bot_cy + inner_r * a.sin()));
                }
                pts.push(Pos2::new(inner.min.x, top_cy));
                for i in 0..=N {
                    let a = a0 * i as f32 / N as f32;
                    pts.push(Pos2::new(
                        cx - inner_r * a.cos(),
                        top_cy - inner_r * a.sin(),
                    ));
                }
            } else {
                pts.push(Pos2::new(inner.min.x, fill_top));
                pts.push(Pos2::new(inner.max.x, fill_top));
                for i in 0..=N {
                    let a = std::f32::consts::PI * i as f32 / N as f32;
                    pts.push(Pos2::new(cx + inner_r * a.cos(), bot_cy + inner_r * a.sin()));
                }
            }
            painter.add(Shape::convex_polygon(
                pts,
                WHITE.gamma_multiply(alpha),
                Stroke::NONE,
            ));
        }

        // Speaker glyph (DrawSpeakerIcon) at the bottom icon zone.
        let icon_c = Pos2::new(track.center().x, track.max.y - VOL_ICON_ZONE * 0.5 * s);
        let (bw, bh, cone) = (6.0 * s, 8.0 * s, 4.5 * s);
        painter.add(Shape::convex_polygon(
            vec![
                Pos2::new(icon_c.x - bw * 0.5 - cone, icon_c.y - bh * 0.3),
                Pos2::new(icon_c.x - bw * 0.5, icon_c.y - bh * 0.5),
                Pos2::new(icon_c.x + bw * 0.5, icon_c.y - bh * 0.5),
                Pos2::new(icon_c.x + bw * 0.5, icon_c.y + bh * 0.5),
                Pos2::new(icon_c.x - bw * 0.5, icon_c.y + bh * 0.5),
                Pos2::new(icon_c.x - bw * 0.5 - cone, icon_c.y + bh * 0.3),
            ],
            VOL_ICON.gamma_multiply(alpha * VOL_ICON_A),
            Stroke::NONE,
        ));
        for (r, a0, a1) in [(bh * 0.75, -50.0f32, 50.0f32), (bh * 1.25, -60.0, 60.0)] {
            let pts: Vec<Pos2> = (0..=10)
                .map(|i| {
                    let a = (a0 + (a1 - a0) * i as f32 / 10.0).to_radians();
                    Pos2::new(
                        icon_c.x + bw * 0.5 + r * a.cos(),
                        icon_c.y + r * a.sin(),
                    )
                })
                .collect();
            painter.add(Shape::line(
                pts,
                Stroke::new(1.6 * s, VOL_ICON.gamma_multiply(alpha * VOL_ICON_A)),
            ));
        }
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
fn icon_album_list(painter: &egui::Painter, rect: Rect, open: bool, alpha: f32) {
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

fn icon_stop(painter: &egui::Painter, rect: Rect, alpha: f32) {
    let k = rect.height() / 100.0;
    painter.rect_filled(
        cv_rect(rect, 100.0, 20.0, 20.0, 60.0, 60.0),
        CornerRadius::same((8.0 * k) as u8),
        WHITE.gamma_multiply(alpha),
    );
}

fn icon_prev(painter: &egui::Painter, rect: Rect, alpha: f32) {
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

fn icon_play(painter: &egui::Painter, rect: Rect, playing: bool, alpha: f32) {
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

fn icon_next(painter: &egui::Painter, rect: Rect, alpha: f32) {
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
fn icon_repeat(painter: &egui::Painter, rect: Rect, mode: RepeatMode, alpha: f32) {
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
fn icon_layout(painter: &egui::Painter, rect: Rect, mode: u8, alpha: f32) {
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
