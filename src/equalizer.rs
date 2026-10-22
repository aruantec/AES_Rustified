//! The AES-mini style 10-band equalizer: a drawer that opens between
//! the transport band and the waveform bar (the band lifts on the same
//! CubicEaseInOut the album list uses — Lacrima's MiniEqualizerView in
//! a MaxHeightToggleBehavior row).
//!
//! The control mirrors EqualizerCompositionVisualHandler: one knob per
//! band on a vertical track, the knob positions joined by a polyline
//! with a soft fill under the curve, left-side "10dB/0dB/-10dB" labels
//! and a frequency label under each track. Input mirrors
//! TryUpdateGain — the pointer grabs the *nearest* band by x, so a
//! horizontal drag paints the whole curve in one stroke.

use eframe::egui::epaint::Vertex;
use eframe::egui::{
    self, Align2, Color32, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke,
};

/// Drawer height in virtual px (MiniEqualizerView Height=180, Margin=5).
pub const PANEL_H: f32 = 340.0;

/// Display range — the visual handler's MinGain/MaxGain (the DSP
/// accepts a little more; the UI stays on Lacrima's ±10).
const MIN_DB: f32 = -10.0;
const MAX_DB: f32 = 10.0;

/// Frequency labels for `audio::EQ_BANDS` (31..16k ISO centers).
const LABELS: [&str; 10] = [
    "31", "62", "125", "250", "500", "1K", "2K", "4K", "8K", "16K",
];

// Visual-handler palette (tint rgb(90,140,200), active rgb(100,180,255)).
const TINT: Color32 = Color32::from_rgb(90, 140, 200);
const ACTIVE: Color32 = Color32::from_rgb(100, 180, 255);
const CURVE: Color32 = Color32::from_rgb(120, 190, 255);
const TEXT: Color32 = Color32::from_rgb(210, 210, 210);

/// (TINT, ACTIVE, CURVE) derived from one base color — the mini shell
/// tints the drawer with the playing cover's palette, so the muted
/// track color and the two bright shades keep the same offsets from
/// the base as the defaults do.
fn shades(base: Color32) -> (Color32, Color32, Color32) {
    let lighten = |c: Color32, f: f32| {
        let [r, g, b, _] = c.to_array();
        Color32::from_rgb(
            (r as f32 + (255.0 - r as f32) * f) as u8,
            (g as f32 + (255.0 - g as f32) * f) as u8,
            (b as f32 + (255.0 - b as f32) * f) as u8,
        )
    };
    (base.gamma_multiply(0.6), lighten(base, 0.3), lighten(base, 0.45))
}

struct BandGeom {
    /// Knob center per band.
    points: [Pos2; 10],
    /// First/last knob x and the vertical track span — needed again
    /// for pointer mapping.
    x0: f32,
    step: f32,
    track_top: f32,
    track_bottom: f32,
    knob_r: f32,
}

/// The drawer's content inside `rect` — which is the already-animated,
/// possibly partially revealed strip. Layout is computed in a
/// full-height strip anchored to `rect`'s bottom and clipped to `rect`,
/// so the lifting transport band uncovers it like a drawer rather than
/// the bars squashing into the growing space.
///
/// Returns true while any band gain is changing (drags, double-click
/// resets), so the caller can push gains to the DSP and persist.
pub fn panel(
    ui: &mut egui::Ui,
    rect: Rect,
    bands: &mut [f32; 10],
    alpha: f32,
    interactive: bool,
    s: f32,
    tint: Option<Color32>,
) -> bool {
    let (tint, active, curve) = tint.map(shades).unwrap_or((TINT, ACTIVE, CURVE));
    // Full-size layout rect, bottom-anchored into the animated strip.
    let full = Rect::from_min_max(
        Pos2::new(rect.min.x, rect.max.y - PANEL_H * s),
        rect.max,
    );
    let painter = ui.painter().with_clip_rect(rect);
    let font = FontId::proportional(10.5 * s);

    // Left dB label block (right-aligned "10dB/0dB/-10dB") then the
    // slider span — the same paddings as the visual handler scaled in.
    let track_top = full.min.y + 14.0 * s;
    let track_bottom = full.max.y - (16.0 * s + 8.0 * s);
    let label_x = full.min.x + 16.0 * s + 40.0 * s;
    for (text, y) in [
        ("10dB", track_top),
        ("0dB", (track_top + track_bottom) * 0.5),
        ("-10dB", track_bottom),
    ] {
        painter.text(
            Pos2::new(label_x, y),
            Align2::RIGHT_CENTER,
            text,
            font.clone(),
            TEXT.gamma_multiply(0.7 * alpha),
        );
    }
    let x0 = label_x + 40.0 * s;
    let x1 = full.max.x - 24.0 * s;
    let step = (x1 - x0).max(1.0) / 9.0;
    let knob_r = (step * 0.18).clamp(5.0 * s, 11.0 * s);

    let mut points = [Pos2::ZERO; 10];
    for (i, db) in bands.iter().enumerate() {
        let t = (db.clamp(MIN_DB, MAX_DB) - MIN_DB) / (MAX_DB - MIN_DB);
        points[i] = Pos2::new(
            x0 + step * i as f32,
            track_bottom - t * (track_bottom - track_top),
        );
    }
    let geom = BandGeom {
        points,
        x0,
        step,
        track_top,
        track_bottom,
        knob_r,
    };

    // Input mirrors TryUpdateGain: a press/drag anywhere in the strip
    // snaps to the nearest band by x and maps y to gain — a horizontal
    // sweep reshapes the curve in one stroke. Double-click zeroes the
    // band under the pointer.
    let mut changed = false;
    let mut active_band: Option<usize> = None;
    let resp = ui.interact(rect, ui.scope_id().with("eq-drawer"), Sense::click_and_drag());
    if interactive {
        let pointer = ui.ctx().input(|i| i.pointer.interact_pos());
        if resp.is_pointer_button_down_on() {
            if let Some(p) = pointer {
                let nearest = (((p.x - geom.x0).max(0.0) / geom.step).round() as usize).min(9);
                let cy = p.y.clamp(geom.track_top, geom.track_bottom);
                let gain = MIN_DB
                    + ((geom.track_bottom - cy) / (geom.track_bottom - geom.track_top).max(1.0))
                        * (MAX_DB - MIN_DB);
                if (bands[nearest] - gain).abs() > 0.001 {
                    bands[nearest] = gain;
                    changed = true;
                }
                active_band = Some(nearest);
            }
        } else if resp.double_clicked() {
            if let Some(p) = ui.ctx().input(|i| i.pointer.latest_pos()) {
                let nearest = (((p.x - geom.x0).max(0.0) / geom.step).round() as usize).min(9);
                if bands[nearest] != 0.0 {
                    bands[nearest] = 0.0;
                    changed = true;
                }
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    // --- Paint (visual handler order): curve fill, curve line, then
    // per-band tracks, knobs and labels ------------------------------

    // Fill under the knob polyline down to the track bottoms — built
    // as a triangle strip per segment since the curve isn't convex.
    if (rect.width() as i32) > 0 && (rect.height() as i32) > 0 {
        let mut mesh = Mesh::default();
        let fill = tint.gamma_multiply(0.12 * alpha);
        for i in 0..9 {
            let base = mesh.vertices.len() as u32;
            let (a, b) = (geom.points[i], geom.points[i + 1]);
            for p in [
                Pos2::new(a.x, geom.track_bottom),
                Pos2::new(a.x, a.y),
                Pos2::new(b.x, b.y),
                Pos2::new(b.x, geom.track_bottom),
            ] {
                mesh.vertices.push(Vertex {
                    pos: p,
                    uv: Pos2::ZERO,
                    color: fill,
                });
            }
            mesh.indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        painter.add(Shape::mesh(mesh));
        // The 3px connecting line through the knobs.
        painter.add(Shape::line(
            geom.points.to_vec(),
            Stroke::new(3.0 * s, curve.gamma_multiply(0.86 * alpha)),
        ));
    }

    for (i, &p) in geom.points.iter().enumerate() {
        // Faint hint track above the knob; solid segment below it.
        painter.line_segment(
            [Pos2::new(p.x, geom.track_top), p],
            Stroke::new(2.0 * s, tint.gamma_multiply(0.16 * alpha)),
        );
        painter.circle_filled(
            Pos2::new(p.x, geom.track_bottom),
            2.5 * s,
            tint.gamma_multiply(0.35 * alpha),
        );
        painter.line_segment(
            [Pos2::new(p.x, geom.track_bottom), p],
            Stroke::new(4.0 * s, active.gamma_multiply(0.86 * alpha)),
        );
        // Pressed knob gets the double glow halo.
        if active_band == Some(i) {
            painter.circle_stroke(
                p,
                geom.knob_r + 5.0 * s,
                Stroke::new(6.0 * s, active.gamma_multiply(0.3 * alpha)),
            );
        }
        painter.circle_filled(p, geom.knob_r, active.gamma_multiply(alpha));
        // Frequency label under the track.
        painter.text(
            Pos2::new(p.x, geom.track_bottom + 14.0 * s),
            Align2::CENTER_CENTER,
            LABELS[i],
            font.clone(),
            TEXT.gamma_multiply(0.7 * alpha),
        );
    }
    changed
}
