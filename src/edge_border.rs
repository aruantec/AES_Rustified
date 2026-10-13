//! Edge border: a glowing ring just inside the window border — a dim
//! cyan/violet gradient with a bright highlight that travels clockwise
//! around the perimeter like the chat input glow. Painted as one
//! vertex-colored mesh: radial alpha falloff across the ring gives the
//! glow spread without needing a blur pass.

use eframe::egui::{self, Color32, Painter, Pos2, Rect, Vec2};

const CYAN: [f32; 3] = [0.10, 0.81, 1.0]; // #19CFFF
const VIOLET: [f32; 3] = [0.43, 0.24, 1.0]; // #6E3EFF
const LEAD: [f32; 3] = [0.12, 0.95, 1.0]; // #1EF2FF
const TAIL: [f32; 3] = [0.60, 0.34, 1.0]; // #9A56FF

const BASE_OPACITY: f32 = 0.4;
/// How far the glow spreads inward from the border.
const GLOW_INWARD: f32 = 14.0;
/// Perimeter loops per second (one loop ≈ 8s).
const SPEED: f32 = 0.12;
/// Comet tail as a fraction of the perimeter.
const TAIL_FRAC: f32 = 0.55;
/// Short bright lead ahead of the head, fraction of the perimeter.
const LEAD_FRAC: f32 = 0.02;

/// (distance inward, alpha multiplier) rings of the glow falloff.
const RINGS: [(f32, f32); 5] = [
    (0.0, 1.0),
    (1.4, 0.55),
    (4.5, 0.22),
    (9.0, 0.06),
    (14.0, 0.0),
];

const _: () = assert!(RINGS[4].0 == GLOW_INWARD);

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Point + outward normal at distance `d` along the rounded-rect
/// perimeter, walked clockwise from the top-left corner.
fn perimeter_point(rect: Rect, r: f32, d: f32) -> (Pos2, Vec2) {
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let straight_w = (rect.width() - 2.0 * r).max(0.0);
    let straight_h = (rect.height() - 2.0 * r).max(0.0);
    let arc = std::f32::consts::FRAC_PI_2 * r;
    let segs = [straight_w, arc, straight_h, arc, straight_w, arc, straight_h, arc];
    let mut d = d;
    let mut i = 0;
    while i < 7 && d > segs[i] {
        d -= segs[i];
        i += 1;
    }
    let ang = |c: Pos2, a: f32| (c + Vec2::new(a.cos(), a.sin()) * r, Vec2::new(a.cos(), a.sin()));
    match i {
        0 => (Pos2::new(rect.min.x + r + d, rect.min.y), Vec2::new(0.0, -1.0)),
        1 => ang(
            Pos2::new(rect.max.x - r, rect.min.y + r),
            -std::f32::consts::FRAC_PI_2 + d / r.max(0.001),
        ),
        2 => (Pos2::new(rect.max.x, rect.min.y + r + d), Vec2::new(1.0, 0.0)),
        3 => ang(
            Pos2::new(rect.max.x - r, rect.max.y - r),
            d / r.max(0.001),
        ),
        4 => (Pos2::new(rect.max.x - r - d, rect.max.y), Vec2::new(0.0, 1.0)),
        5 => ang(
            Pos2::new(rect.min.x + r, rect.max.y - r),
            std::f32::consts::FRAC_PI_2 + d / r.max(0.001),
        ),
        6 => (Pos2::new(rect.min.x, rect.max.y - r - d), Vec2::new(-1.0, 0.0)),
        _ => ang(
            Pos2::new(rect.min.x + r, rect.min.y + r),
            std::f32::consts::PI + d / r.max(0.001),
        ),
    }
}

/// Total rounded-rect perimeter length.
fn perimeter_len(rect: Rect, r: f32) -> f32 {
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0);
    2.0 * (rect.width() - 2.0 * r).max(0.0)
        + 2.0 * (rect.height() - 2.0 * r).max(0.0)
        + 2.0 * std::f32::consts::PI * r
}

/// Paint the traveling glow inside `rect`; the caller shrinks the rect
/// and radius by the same inset so the ring stays concentric with the
/// window border (outermost glow ring lands on the border line).
pub fn paint(painter: &Painter, rect: Rect, radius: f32, elapsed: f32) {
    let len = perimeter_len(rect, radius);
    if len <= 0.0 {
        return;
    }
    let samples = ((len / 7.0) as usize).clamp(160, 720);
    let head = (elapsed * SPEED).fract();

    let mut mesh = egui::epaint::Mesh::default();
    mesh.vertices.reserve((samples + 1) * RINGS.len());
    mesh.indices.reserve(samples * (RINGS.len() - 1) * 6);

    for i in 0..=samples {
        let s = i as f32 / samples as f32;
        let (p, n) = perimeter_point(rect, radius, s * len);
        // Circular distance from the traveling head; d < 0 is behind it.
        let mut d = s - head;
        d -= d.round();
        let (hi, mix_t) = if d < 0.0 {
            let t = 1.0 + d / TAIL_FRAC;
            if t > 0.0 { (t * t, t) } else { (0.0, 0.0) }
        } else if d < LEAD_FRAC {
            (1.0 - d / LEAD_FRAC, 1.0)
        } else {
            (0.0, 0.0)
        };
        // Wrap-continuous cyan -> violet -> cyan base gradient.
        let tri = 1.0 - (2.0 * s - 1.0).abs();
        let base = lerp3(CYAN, VIOLET, tri);
        let col = lerp3(base, lerp3(TAIL, LEAD, mix_t), hi);
        let alpha = (BASE_OPACITY + hi * (1.0 - BASE_OPACITY)).min(1.0);
        let (r8, g8, b8) = (
            (col[0] * 255.0) as u8,
            (col[1] * 255.0) as u8,
            (col[2] * 255.0) as u8,
        );
        for &(dist, fall) in RINGS.iter() {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: p - n * dist,
                uv: egui::epaint::WHITE_UV,
                color: Color32::from_rgba_unmultiplied(
                    r8,
                    g8,
                    b8,
                    (alpha * fall * 255.0) as u8,
                ),
            });
        }
    }

    for i in 0..samples {
        for k in 0..(RINGS.len() - 1) {
            let a = (i * RINGS.len() + k) as u32;
            let b = a + RINGS.len() as u32;
            mesh.indices.extend_from_slice(&[
                a,
                b,
                a + 1,
                b,
                b + 1,
                a + 1,
            ]);
        }
    }
    painter.add(egui::epaint::Shape::Mesh(std::sync::Arc::new(mesh)));
}
