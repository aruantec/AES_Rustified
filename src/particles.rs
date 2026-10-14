//! Lacrima rain: the small tapered streaks that drift down the window,
//! ported from AES_Controls' `ParticleVisualHandler` (the "Show Lacrima"
//! effect). Drawn as a single vertex-colored mesh — the original's
//! fragment-shader gradient becomes per-vertex alpha.

use eframe::egui::{self, Color32, Painter, Pos2, Rect};

/// Count slider range, matching Lacrima's settings UI (10 ..= 60000).
pub const MIN_COUNT: u32 = 10;
pub const MAX_COUNT: u32 = 60_000;

#[derive(Clone, Copy)]
struct Drop {
    /// Position in normalized space ([-1, 1], y up) like the original.
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    /// Streak width/length in pixels.
    width: f32,
    length: f32,
    /// White luminance (0.88..1.0) and alpha.
    lum: f32,
    alpha: f32,
}

pub struct LacrimaParticles {
    drops: Vec<Drop>,
    rng: u64,
}

impl Default for LacrimaParticles {
    fn default() -> Self {
        Self::new()
    }
}

impl LacrimaParticles {
    pub fn new() -> Self {
        Self {
            drops: Vec::new(),
            rng: 0x2545_F491_4F2C_DD1D,
        }
    }

    /// Resize the swarm; a changed count respawns every drop, like the
    /// original `ResetParticles`.
    pub fn set_count(&mut self, count: u32) {
        let count = count.clamp(MIN_COUNT, MAX_COUNT) as usize;
        if self.drops.len() == count {
            return;
        }
        self.drops.clear();
        self.drops.reserve(count);
        for _ in 0..count {
            let drop = self.spawn(true);
            self.drops.push(drop);
        }
    }

    /// xorshift64 in [0, 1).
    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        ((self.rng >> 40) & 0xFF_FFFF) as f32 / 0x1_000_000 as f32
    }

    fn spawn(&mut self, anywhere: bool) -> Drop {
        let speed = self.rand() * 2.8 + 1.6;
        Drop {
            x: self.rand() * 2.0 - 1.0,
            y: if anywhere {
                self.rand() * 2.2 - 1.1
            } else {
                1.05 + self.rand() * 0.15
            },
            vx: (self.rand() - 0.5) * 0.0009,
            vy: -speed * 0.00135,
            width: self.rand() * 1.6 + 0.7,
            length: speed * (self.rand() * 4.5 + 3.5),
            lum: self.rand() * 0.12 + 0.88,
            alpha: self.rand() * 0.2 + 0.12,
        }
    }

    pub fn update(&mut self, dt: f32) {
        let tf = dt * 60.0;
        for i in 0..self.drops.len() {
            let mut p = self.drops[i];
            p.x += p.vx * tf;
            p.y += p.vy * tf;
            if p.y < -1.15 {
                p = self.spawn(false);
            } else if p.x < -1.12 {
                p.x = 1.12;
            } else if p.x > 1.12 {
                p.x = -1.12;
            }
            self.drops[i] = p;
        }
    }

    /// Every drop is a tapered pentagon (narrow tail top, wide shoulders,
    /// pointed tip) plus a center vertex so the fan interpolates the same
    /// soft gradient as the original fragment shader: alpha(u,v) =
    /// horiz * mix(tail, 1, head) * a. `tint` — the cover palette —
    /// colors drops round-robin like the shadertoy gradients.
    pub fn paint(&self, painter: &Painter, rect: Rect, tint: Option<&[egui::Color32]>) {
        if self.drops.is_empty() || rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        let mut mesh = egui::epaint::Mesh::default();
        mesh.vertices.reserve(self.drops.len() * 6);
        mesh.indices.reserve(self.drops.len() * 15);
        let (w, h) = (rect.width(), rect.height());
        for (i, p) in self.drops.iter().enumerate() {
            let px = rect.min.x + (p.x + 1.0) * 0.5 * w;
            let py = rect.min.y + (1.0 - (p.y + 1.0) * 0.5) * h;
            let half_w = (p.width * 0.5).max(0.45);
            let len = p.length.max(4.0);
            let top_hw = half_w * 0.3;
            let bot_hw = half_w * 1.25;
            let top_y = py - len * 0.5;
            let head_y = py + len * 0.22;
            let tip_y = py + len * 0.5 + half_w * 0.35;
            let u_of = |x: f32| (x - px + bot_hw) / (2.0 * bot_hw);
            let (r8, g8, b8) = match tint {
                Some(pal) if !pal.is_empty() => {
                    // Cover-colored drops — the palette luminance is
                    // already encoded, `lum` keeps per-drop variation.
                    let c = pal[i % pal.len()];
                    (
                        (c.r() as f32 * p.lum) as u8,
                        (c.g() as f32 * p.lum) as u8,
                        (c.b() as f32 * p.lum) as u8,
                    )
                }
                _ => {
                    let lum = (p.lum * 255.0) as u8;
                    (lum, lum, lum)
                }
            };

            let base = mesh.vertices.len() as u32;
            let vert = |mesh: &mut egui::epaint::Mesh, x: f32, y: f32, u: f32, v: f32| {
                mesh.vertices.push(egui::epaint::Vertex {
                    pos: Pos2::new(x, y),
                    uv: egui::epaint::WHITE_UV,
                    color: Color32::from_rgba_unmultiplied(
                        r8,
                        g8,
                        b8,
                        (drop_alpha(u, v, p.alpha) * 255.0) as u8,
                    ),
                });
            };
            vert(&mut mesh, px - top_hw, top_y, u_of(px - top_hw), 0.0); // tail L
            vert(&mut mesh, px + top_hw, top_y, u_of(px + top_hw), 0.0); // tail R
            vert(&mut mesh, px + bot_hw, head_y, 1.0, 0.78); // head R
            vert(&mut mesh, px, tip_y, 0.5, 1.0); // tip
            vert(&mut mesh, px - bot_hw, head_y, 0.0, 0.78); // head L
            vert(&mut mesh, px, head_y, 0.5, 0.78); // fan hub at head center
            for i in 0..5u32 {
                mesh.indices
                    .extend_from_slice(&[base + 5, base + i, base + (i + 1) % 5]);
            }
        }
        painter.add(egui::epaint::Shape::Mesh(std::sync::Arc::new(mesh)));
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Same alpha profile as the GL fragment shader: horizontal falloff,
/// bright head toward the tip (v=1), soft tail (v=0).
fn drop_alpha(u: f32, v: f32, a: f32) -> f32 {
    let across = (u - 0.5).abs() * 2.0;
    let horiz = 1.0 - smoothstep(0.12, 1.0, across);
    let head = smoothstep(0.55, 1.0, v);
    let tail = 1.0 - smoothstep(0.2, 1.0, 1.0 - v);
    horiz * (tail + (1.0 - tail) * head) * a
}
