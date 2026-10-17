//! Cyberpunk title logo, a port of AES_Lacrima's
//! `TitleLogoCompositionVisualHandler`: RGB-split title with a periodic
//! glitch burst (channel split, horizontal slices, per-char jitter, flash).

use eframe::egui::{self, Align2, Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Vec2};

const GLITCH_CHARSET: &str = "█▓▒░0123456789@#$%&*!?/\\|";
pub const CONSOLAS_BOLD_FAMILY: &str = "ConsolasBold";

struct Slice {
    y: f32,
    h: f32,
    dx: f32,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f32()
    }

    fn int(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() % (hi - lo + 1) as u64) as u32
    }

    fn chance(&mut self, p: f32) -> bool {
        self.f32() < p
    }
}

fn hash01(seed: u64, i: u64) -> f32 {
    let mut z = seed.wrapping_add(i.wrapping_mul(0x9E3779B97F4A7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

pub struct TitleLogo {
    rng: Rng,
    /// Glitch steps left in the current burst.
    glitch_frames: u32,
    /// Seconds until the current step re-rolls (Cyberpunk stutter:
    /// each corrupted state holds, then snaps to the next).
    step_timer: f32,
    next_burst_in: f32,
    display: String,
    offset: Vec2,
    rgb_split: f32,
    flash: bool,
    flash_cyan: bool,
    jitter_seed: u64,
    slices: Vec<Slice>,
    have_font: bool,
}

impl TitleLogo {
    pub fn new(have_font: bool) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0x1234);
        let mut rng = Rng(seed | 1);
        let next_burst_in = rng.range(1.0, 3.0);
        Self {
            rng,
            glitch_frames: 0,
            step_timer: 0.0,
            next_burst_in,
            display: String::new(),
            offset: Vec2::ZERO,
            rgb_split: 2.5,
            flash: false,
            flash_cyan: false,
            jitter_seed: 0,
            slices: Vec::new(),
            have_font,
        }
    }

    /// Whether a glitch burst is in progress — the caller must keep
    /// repainting while this is true, otherwise the last scrambled
    /// frame stays on screen forever (egui only repaints on demand).
    pub fn glitching(&self) -> bool {
        self.glitch_frames > 0
    }

    /// Advance animation state. Call once per frame; `dt` in seconds.
    /// A burst is 5–9 discrete steps of ~40–110ms each — the glitch
    /// holds frozen inside a step and snaps to a new state at each
    /// step boundary, like Cyberpunk 2077's title glitch.
    pub fn advance(&mut self, dt: f32, title: &str) {
        if self.glitch_frames == 0 {
            self.next_burst_in -= dt;
            if self.next_burst_in <= 0.0 {
                self.glitch_frames = self.rng.int(5, 9);
                self.step_timer = 0.0; // roll the first step below
            } else {
                return;
            }
        }

        self.step_timer -= dt;
        if self.step_timer > 0.0 {
            return;
        }
        self.glitch_frames -= 1;
        if self.glitch_frames == 0 {
            self.display = title.to_owned();
            self.offset = Vec2::ZERO;
            self.rgb_split = 2.5;
            self.flash = false;
            self.slices.clear();
            self.next_burst_in = self.rng.range(2.8, 5.5);
            return;
        }

        // New glitch step: everything re-rolls and then holds.
        self.step_timer = self.rng.range(0.04, 0.11);
        self.offset = Vec2::new(self.rng.range(-8.0, 8.0), self.rng.range(-1.25, 1.25));
        self.rgb_split = self.rng.range(6.0, 15.0);
        self.flash = self.rng.chance(0.35);
        self.flash_cyan = self.rng.chance(0.5);
        self.jitter_seed = self.rng.next();
        self.display = if self.rng.chance(0.65) {
            corrupt(title, self.rng.int(4, 8), &mut self.rng)
        } else {
            title.to_owned()
        };
        let n = self.rng.int(2, 4);
        self.slices = (0..n)
            .map(|_| Slice {
                y: self.rng.f32(),
                h: 0.1 + self.rng.f32() * 0.16,
                dx: self.rng.range(-16.0, 16.0),
            })
            .collect();
    }

    /// Paint the title centered in `rect`. `elapsed` in seconds.
    /// With `animate` off (Settings > "Animate title"), render the plain
    /// title — no RGB split or glitch — like Lacrima's static TextBlock.
    pub fn paint(&self, painter: &egui::Painter, rect: Rect, elapsed: f32, title: &str, animate: bool) {
        let glitching = self.glitch_frames > 0;
        let pulse = if glitching {
            1.0
        } else {
            0.82 + 0.18 * (elapsed * 1.8).sin()
        };
        let text_size = (rect.width() / 10.0)
            .min(rect.height() * 0.5)
            .clamp(13.0, 28.0);
        let family = if self.have_font {
            FontFamily::Name(CONSOLAS_BOLD_FAMILY.into())
        } else {
            FontFamily::Monospace
        };
        let font = FontId::new(text_size, family);
        let display = if self.display.is_empty() {
            title
        } else {
            self.display.as_str()
        };

        let galley = painter.layout_no_wrap(display.to_owned(), font.clone(), Color32::WHITE);
        let text_w = galley.rect.width();
        // The title has no descenders, so centering the galley (which
        // reserves descender space in its line height) leaves it
        // visibly high. Center the caps instead: baseline - half cap
        // height is the visual middle.
        let baseline = galley
            .rows
            .first()
            .and_then(|r| r.glyphs.first())
            .map(|g| g.pos.y)
            .unwrap_or(galley.rect.height() * 0.8);
        let cap_h = text_size * 0.72;
        let cap_dy = galley.rect.height() * 0.5 - baseline + cap_h * 0.5;

        // Burst offset only applies while animated — a static title
        // must stay put even if the burst state machine keeps running.
        let (cx, cy) = if animate {
            (rect.center().x + self.offset.x, rect.center().y + self.offset.y + cap_dy)
        } else {
            (rect.center().x, rect.center().y + cap_dy)
        };

        // The whole title renders at 30% opacity.
        const TITLE_OPACITY: f32 = 0.3;
        let shadow = Color32::from_black_alpha(150).gamma_multiply(TITLE_OPACITY);
        let cyan =
            Color32::from_rgba_unmultiplied(0, 245, 255, (110.0 * pulse) as u8)
                .gamma_multiply(TITLE_OPACITY);
        let magenta =
            Color32::from_rgba_unmultiplied(255, 0, 180, (90.0 * pulse) as u8)
                .gamma_multiply(TITLE_OPACITY);
        let main = Color32::from_rgb(
            (225.0 + 30.0 * pulse) as u8,
            (245.0 + 10.0 * pulse) as u8,
            255,
        )
        .gamma_multiply(TITLE_OPACITY);

        painter.text(
            Pos2::new(cx + 1.0, cy + 1.0),
            Align2::CENTER_CENTER,
            title,
            font.clone(),
            shadow,
        );

        if !animate {
            painter.text(
                Pos2::new(cx, cy),
                Align2::CENTER_CENTER,
                title,
                font,
                Color32::from_rgb(245, 250, 255).gamma_multiply(TITLE_OPACITY),
            );
            return;
        }

        if !glitching {
            painter.text(
                Pos2::new(cx - self.rgb_split, cy),
                Align2::CENTER_CENTER,
                display,
                font.clone(),
                magenta,
            );
            painter.text(
                Pos2::new(cx + self.rgb_split, cy),
                Align2::CENTER_CENTER,
                display,
                font.clone(),
                cyan,
            );
            painter.text(
                Pos2::new(cx, cy),
                Align2::CENTER_CENTER,
                display,
                font,
                main,
            );
            return;
        }

        if self.flash {
            let flash_rect = Rect::from_min_max(
                Pos2::new(cx - text_w / 2.0 - 4.0, cy - text_size * 0.72),
                Pos2::new(cx + text_w / 2.0 + 4.0, cy + text_size * 0.18),
            );
            let color = if self.flash_cyan {
                Color32::from_rgba_unmultiplied(0, 240, 255, 38)
            } else {
                Color32::from_rgba_unmultiplied(255, 0, 170, 34)
            }
            .gamma_multiply(TITLE_OPACITY);
            painter.rect_filled(flash_rect, CornerRadius::ZERO, color);
        }

        let rgb = self.rgb_split;
        painter.text(
            Pos2::new(cx - rgb * 1.4, cy - 1.0),
            Align2::CENTER_CENTER,
            display,
            font.clone(),
            magenta,
        );
        painter.text(
            Pos2::new(cx + rgb * 1.4, cy + 1.0),
            Align2::CENTER_CENTER,
            display,
            font.clone(),
            cyan,
        );
        painter.text(
            Pos2::new(cx - rgb * 0.55, cy),
            Align2::CENTER_CENTER,
            display,
            font.clone(),
            magenta,
        );
        painter.text(
            Pos2::new(cx + rgb * 0.55, cy),
            Align2::CENTER_CENTER,
            display,
            font.clone(),
            cyan,
        );

        for slice in &self.slices {
            let top = cy - text_size * 0.62 + slice.y * text_size * 0.72;
            let h = (text_size * slice.h).max(3.0);
            let clip = Rect::from_min_max(
                Pos2::new(cx - text_w / 2.0 - 6.0, top),
                Pos2::new(cx + text_w / 2.0 + 6.0, top + h),
            );
            painter.with_clip_rect(clip).text(
                Pos2::new(cx + slice.dx, cy),
                Align2::CENTER_CENTER,
                display,
                font.clone(),
                cyan,
            );
            painter.with_clip_rect(clip).text(
                Pos2::new(cx + slice.dx * 0.65, cy),
                Align2::CENTER_CENTER,
                display,
                font.clone(),
                main,
            );
        }

        // Per-character jitter with accumulated spacing drift; a random
        // subset of characters gets kicked harder.
        let mut x = cx - text_w / 2.0;
        let mut drift = 0.0f32;
        for (i, c) in display.chars().enumerate() {
            let glyph = c.to_string();
            let strong = hash01(self.jitter_seed, i as u64 + 2000) > 0.6;
            let amp = if strong { 9.0 } else { 5.0 };
            let jx = hash01(self.jitter_seed, i as u64) * amp - amp / 2.0;
            let jy = hash01(self.jitter_seed, i as u64 + 1000) * if strong { 5.0 } else { 2.0 }
                - if strong { 2.5 } else { 1.0 };
            drift += (hash01(self.jitter_seed, i as u64 + 3000) - 0.5) * 2.4;
            painter.text(
                Pos2::new(x + drift + jx, cy + jy),
                Align2::LEFT_CENTER,
                glyph.clone(),
                font.clone(),
                main,
            );
            x += painter
                .layout_no_wrap(glyph, font.clone(), main)
                .rect
                .width();
        }
    }
}

fn corrupt(title: &str, count: u32, rng: &mut Rng) -> String {
    let charset: Vec<char> = GLITCH_CHARSET.chars().collect();
    let mut chars: Vec<char> = title.chars().collect();
    for _ in 0..count {
        if chars.is_empty() {
            break;
        }
        let i = rng.int(0, chars.len() as u32 - 1) as usize;
        if chars[i] == ' ' {
            continue;
        }
        let r = rng.f32();
        if r < 0.65 {
            chars[i] = charset[rng.int(0, charset.len() as u32 - 1) as usize];
        } else if r < 0.85 {
            let c = chars[i];
            chars[i] = if c.is_uppercase() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c.to_uppercase().next().unwrap_or(c)
            };
        } else {
            chars.remove(i);
        }
    }
    if rng.chance(0.3) && !chars.is_empty() {
        let i = rng.int(0, chars.len() as u32 - 1) as usize;
        let c = chars[i];
        if c != ' ' {
            chars.insert(i, c);
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A burst must always play out and restore the real title —
    /// if repaints stop mid-burst the scrambled frame would freeze
    /// on screen forever (the "garbled logo" report).
    #[test]
    fn glitch_burst_always_terminates() {
        let title = "AES Lacrima [Rust]";
        let mut logo = TitleLogo::new(false);
        // 60s at 60fps: whenever a burst is active it must finish
        // within 2s of steady frames (5-9 steps of ≤110ms each) and
        // restore the clean title — a frozen burst leaves scrambled
        // text on screen when repaints stop.
        let mut saw_burst = false;
        for _ in 0..60 * 60 {
            logo.advance(1.0 / 60.0, title);
            if !logo.glitching() {
                continue;
            }
            saw_burst = true;
            for _ in 0..120 {
                logo.advance(1.0 / 60.0, title);
            }
            assert!(!logo.glitching(), "burst never terminated");
            assert_eq!(logo.display, title);
        }
        assert!(saw_burst, "no burst fired in 60s");
    }
}
