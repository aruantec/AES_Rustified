//! Shared painting helpers: rounded-corner image meshes (egui has no
//! rounded clip, so covers are drawn as a triangle-fan mesh that maps
//! UVs linearly over a rounded rect) and UniformToFill UV cropping.

use eframe::egui::epaint::Vertex;
use eframe::egui::{self, Color32, Mesh, Pos2, Rect, TextureId};

/// UV sub-rect that center-crops `src` to fill `dest` (Lacrima's
/// UniformToFillSrc): crop the longer axis, keep the shorter.
pub fn fill_uv(src_size: [usize; 2], dest: Rect) -> Rect {
    let (sw, sh) = (src_size[0] as f32, src_size[1] as f32);
    if sw <= 0.0 || sh <= 0.0 || dest.width() <= 0.0 || dest.height() <= 0.0 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    let src_a = sw / sh;
    let dst_a = dest.width() / dest.height();
    let (mut u0, mut v0, mut u1, mut v1) = (0.0, 0.0, 1.0, 1.0);
    if src_a > dst_a {
        let crop = sh * dst_a / sw;
        u0 = (1.0 - crop) * 0.5;
        u1 = 1.0 - u0;
    } else {
        let crop = sw / dst_a / sh;
        v0 = (1.0 - crop) * 0.5;
        v1 = 1.0 - v0;
    }
    Rect::from_min_max(Pos2::new(u0, v0), Pos2::new(u1, v1))
}

const CORNER_SEGMENTS: usize = 6;

/// Triangle-fan mesh: image `tex` painted into `rect` clipped to a
/// rounded rectangle of `radius`, tinted by `tint` (alpha applies).
pub fn rounded_image_mesh(
    tex: TextureId,
    rect: Rect,
    uv: Rect,
    radius: f32,
    tint: Color32,
) -> Mesh {
    let r = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let mut mesh = Mesh::with_texture(tex);
    let to_uv = |p: Pos2| {
        Pos2::new(
            uv.min.x + (p.x - rect.min.x) / rect.width() * uv.width(),
            uv.min.y + (p.y - rect.min.y) / rect.height() * uv.height(),
        )
    };
    mesh.vertices.push(Vertex {
        pos: rect.center(),
        uv: to_uv(rect.center()),
        color: tint,
    });
    // Ring of perimeter vertices, clockwise in y-down space. Each corner
    // is an arc of CORNER_SEGMENTS+1 points between the edge tangents.
    let corners = [
        (Pos2::new(rect.min.x + r, rect.min.y + r), std::f32::consts::PI),
        (
            Pos2::new(rect.max.x - r, rect.min.y + r),
            std::f32::consts::PI * 1.5,
        ),
        (Pos2::new(rect.max.x - r, rect.max.y - r), 0.0),
        (Pos2::new(rect.min.x + r, rect.max.y - r), std::f32::consts::FRAC_PI_2),
    ];
    for &(center, start) in &corners {
        for s in 0..=CORNER_SEGMENTS {
            let a = start + s as f32 / CORNER_SEGMENTS as f32 * std::f32::consts::FRAC_PI_2;
            let p = Pos2::new(center.x + a.cos() * r, center.y + a.sin() * r);
            let uv = to_uv(p);
            mesh.vertices.push(Vertex {
                pos: p,
                uv,
                color: tint,
            });
        }
    }
    // Perimeter vertices occupy indices 1..=ring (0 is the center).
    let ring = 4 * (CORNER_SEGMENTS + 1) as u32;
    for i in 0..ring - 1 {
        mesh.indices.extend_from_slice(&[0, 1 + i, 2 + i]);
    }
    mesh.indices.extend_from_slice(&[0, ring, 1]);
    mesh
}

/// Convenience: paint a texture into `rect` with rounded corners and
/// fill-cropped UVs. `src_size` is the texture's pixel size.
pub fn paint_cover(
    painter: &egui::Painter,
    tex: TextureId,
    src_size: [usize; 2],
    dest: Rect,
    radius: f32,
    alpha: f32,
) {
    if alpha <= 0.001 {
        return;
    }
    let uv = fill_uv(src_size, dest);
    let tint = Color32::WHITE.gamma_multiply(alpha);
    painter.add(egui::Shape::mesh(rounded_image_mesh(
        tex, dest, uv, radius, tint,
    )));
}
