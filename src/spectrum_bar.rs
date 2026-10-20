//! Music-view spectrum: port of Lacrima's `GlSpectrumControl` +
//! `SpectrumCompositionVisualHandler`. The analyzer (`audio::Spectrum`)
//! supplies 48 smoothed log-band magnitudes; this widget applies the
//! control's display physics (rise cap, attack/release, peak decay,
//! globalMax normalization) and draws the segmented bars from its
//! fixed 960x192 render canvas, stretched to the target rect.

use std::collections::HashMap;

use eframe::egui::{self, Color32, Pos2, Rect};

use crate::audio::{self, AudioPlayer};
use crate::settings;

/// Offscreen canvas the C# control renders into before blitting.
const RENDER_W: f32 = 960.0;
const RENDER_H: f32 = 192.0;
const MAX_BARS: usize = 192;
/// MusicView's `BlockHeight=5`; segment gap is 15% of the block.
const BLOCK_H: f32 = 7.5;
/// Bar width renders at 0.8x the configured value.
const BAR_W_SCALE: f32 = 0.8;

// UpdatePhysics constants (UseDeltaTime=true -> factors scaled by dt*60).
const ATTACK: f32 = 0.42;
const RELEASE: f32 = 0.38;
// Peak-hold decay per 60fps tick — 0.85 collapsed the marker to the
// bar almost instantly; ~0.045 makes it hang, then float down like a
// hardware analyzer's peak segment.
const PEAK_DECAY: f32 = 0.045;
const PRE_POW: f32 = 0.90;
const MAX_RISE_FRAC: f32 = 0.55;
const MAX_RISE_ABS: f32 = 0.05;

/// `GetTargetBarCount`: derived from the fixed render width and the
/// configured bar pitch, capped at MaxBarCount.
fn bar_count(bar_width: f32, bar_spacing: f32) -> usize {
    let pitch = (bar_width * BAR_W_SCALE + bar_spacing).max(1.0);
    ((RENDER_W / pitch) as usize).clamp(1, MAX_BARS)
}

pub struct SpectrumBar {
    analyzer: Option<audio::Spectrum>,
    snapshot: Vec<f32>,
    /// Raw FFT bins for the ShaderToy `iChannel0` texture — what
    /// `AudioPlayer.Spectrum` feeds `GlShaderToyControl.Spectrum`.
    shader_snapshot: Vec<f32>,
    displayed: Vec<f32>,
    peaks: Vec<f32>,
    raw: Vec<f32>,
    sample_map: Vec<usize>,
    map_src: usize,
    global_max: f32,
    first_frame: bool,
}

impl SpectrumBar {
    pub fn new() -> Self {
        Self {
            analyzer: None,
            snapshot: Vec::new(),
            shader_snapshot: Vec::new(),
            displayed: Vec::new(),
            peaks: Vec::new(),
            raw: Vec::new(),
            sample_map: Vec::new(),
            map_src: usize::MAX,
            global_max: 0.05,
            first_frame: true,
        }
    }

    /// Drain the player's spectrum tap, run the FFT, and advance the
    /// display physics by `dt`. Returns true while bars are still
    /// settling (repaint needed). Call once per frame.
    pub fn update(
        &mut self,
        player: Option<&mut AudioPlayer>,
        dt: f32,
        cfg: &settings::Spectrum,
    ) -> bool {
        self.snapshot.clear();
        if let Some(p) = player {
            if self.analyzer.is_none() {
                self.analyzer = Some(audio::Spectrum::new(p.output_rate() as f32));
            }
            let a = self.analyzer.as_mut().unwrap();
            a.feed(&p.drain_spectrum());
            self.snapshot.extend_from_slice(a.bars());
            self.shader_snapshot.clear();
            self.shader_snapshot.extend_from_slice(a.shader_bins());
        }

        // Bar count follows the configured pitch; resizing the physics
        // arrays re-triggers the first-frame snap like the C# control.
        let count = bar_count(cfg.bar_width, cfg.bar_spacing);
        if self.displayed.len() != count {
            self.displayed = vec![0.0; count];
            self.peaks = vec![0.0; count];
            self.raw = vec![0.0; count];
            self.sample_map = vec![0; count];
            self.map_src = usize::MAX;
            self.first_frame = true;
        }

        // Map each display bar onto a source bin (snapshot is finer or
        // coarser than the bar count depending on the analyzer).
        if self.map_src != self.snapshot.len() {
            let src = self.snapshot.len();
            for (i, m) in self.sample_map.iter_mut().enumerate() {
                *m = if src > 0 {
                    (((i as f32 / count as f32) * src as f32) as usize).min(src - 1)
                } else {
                    0
                };
            }
            self.map_src = src;
        }

        let tf = dt * 60.0;
        let adj_attack = 1.0 - (1.0 - ATTACK).powf(tf);
        let adj_release = 1.0 - (1.0 - RELEASE).powf(tf);
        let adj_peak_decay = 1.0 - (1.0 - PEAK_DECAY).powf(tf);

        let mut observed_max = 0.0f32;
        let mut activity = false;
        for i in 0..count {
            let src = self
                .snapshot
                .get(self.sample_map[i])
                .copied()
                .unwrap_or(0.0);
            let src = if src.is_finite() && src >= 0.0001 { src } else { 0.0 };
            self.raw[i] += (src - self.raw[i]) * (PRE_POW * tf).min(1.0);
            let target = self.raw[i];

            if self.first_frame && target > 0.0 {
                self.displayed[i] = target;
                self.peaks[i] = target;
            } else {
                // Rise is capped per-frame so bars can't teleport up.
                let effective = if target > self.displayed[i] {
                    (self.displayed[i]
                        + (target * MAX_RISE_FRAC).max(MAX_RISE_ABS) * tf)
                        .min(target)
                } else {
                    target
                };
                let lerp = if effective < self.displayed[i] {
                    adj_release
                } else {
                    adj_attack
                };
                self.displayed[i] += (effective - self.displayed[i]) * lerp;

                if self.displayed[i] > self.peaks[i] {
                    self.peaks[i] = self.displayed[i];
                } else {
                    self.peaks[i] += (self.displayed[i] - self.peaks[i]) * adj_peak_decay;
                }
            }
            observed_max = observed_max.max(self.displayed[i]);
            if self.displayed[i] > 0.001 || self.peaks[i] > 0.001 || self.raw[i] > 0.001 {
                activity = true;
            }
        }

        if observed_max > 0.001 {
            if self.first_frame {
                self.global_max = observed_max;
                self.first_frame = false;
            } else {
                // Fast follow-up, slow decay: quiet passages keep scale.
                let l = if observed_max > self.global_max { 0.15 } else { 0.01 };
                self.global_max += (observed_max - self.global_max) * (l * tf).min(1.0);
            }
        }
        self.global_max = self.global_max.max(0.05);

        activity || observed_max > 0.001 || self.global_max > 0.051
    }

    /// Raw FFT bins for the Shadertoy `iChannel0` spectrum texture —
    /// the same role as `GlShaderToyControl.Spectrum` on the C#
    /// control (which binds `AudioPlayer.Spectrum`, not the display
    /// bars). Empty until the analyzer runs.
    pub fn shader_bins(&self) -> &[f32] {
        &self.shader_snapshot
    }

    /// Draw the bars bottom-aligned in `rect` with `colors` as the
    /// horizontal gradient (evenly-spaced stops). Pure paint — the
    /// control is `IsHitTestVisible=False`.
    pub fn paint(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        cfg: &settings::Spectrum,
        colors: &[Color32],
        alpha: f32,
    ) {
        if alpha <= 0.001 || colors.is_empty() {
            return;
        }
        let count = self.displayed.len();
        if count == 0 {
            return;
        }
        let denom = self.global_max * 1.1 + 1e-9;
        let sx = rect.width() / RENDER_W;
        let sy = rect.height() / RENDER_H;
        // Pixel-quantized grid: bar, gap, and slab heights are whole
        // pixels — a fractional pitch lands every column on a
        // different subpixel and the AA makes the gaps jitter.
        let bar_w = (cfg.bar_width * BAR_W_SCALE * sx).round().max(1.0);
        let spacing = (cfg.bar_spacing * sx).round().max(2.0);
        let pitch = bar_w + spacing;
        let total_w = count as f32 * bar_w + (count - 1) as f32 * spacing;
        let mut x = rect.min.x + ((rect.width() - total_w).max(0.0) * 0.5).round();
        let block_h = (BLOCK_H * sy).round().max(3.0);
        let gap = (block_h * 0.35).round().max(1.0);
        // Segment thickness is half the lit part of the cell — thin
        // VFD slabs with the same row pitch and gap.
        let slab_h = ((block_h - gap) * 0.5).round().max(1.0);
        let bottom = rect.max.y.round();

        for i in 0..count {
            let x0 = x;
            let x1 = (x0 + bar_w).min(rect.max.x);
            if x1 <= x0 || x0 >= rect.max.x {
                break;
            }
            let level = (self.displayed[i] / denom).clamp(0.0, 1.0);
            let peak = (self.peaks[i] / denom).clamp(0.0, 1.0);
            let color = gradient_color(colors, i, count);
            self.paint_column(
                painter,
                x0,
                x1,
                rect.min.y,
                bottom,
                rect.height(),
                level,
                peak,
                color,
                alpha,
                block_h,
                gap,
                slab_h,
            );
            x += pitch;
        }
    }

    /// One VFD column: faint unlit-segment grid, the lit stack rising
    /// from `bottom` over `area_h`, and the peak marker snapped to a
    /// cell. Shared by `paint` and `paint_mini` so the two strips look
    /// identical at equal virtual size.
    #[allow(clippy::too_many_arguments)]
    fn paint_column(
        &self,
        painter: &egui::Painter,
        x0: f32,
        x1: f32,
        top_y: f32,
        bottom: f32,
        area_h: f32,
        level: f32,
        peak: f32,
        color: Color32,
        alpha: f32,
        block_h: f32,
        gap: f32,
        slab_h: f32,
    ) {
        let bar_top = bottom - level * area_h;

        // Ghost grid: the column's unlit segments faintly visible
        // behind the lit stack.
        let mut gcur = bottom;
        while gcur - gap > top_y {
            let top = (gcur - gap - slab_h).max(top_y);
            let vis_bottom = gcur - gap;
            if vis_bottom > top {
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, vis_bottom)),
                    0.0,
                    color.gamma_multiply(0.06 * alpha),
                );
            }
            gcur -= block_h;
        }

        // Lit stack, brighter toward the top (0.65 + 0.35 * height).
        let mut cur = bottom;
        while cur - gap > bar_top {
            let top = (cur - gap - slab_h).max(bar_top);
            let vis_bottom = cur - gap;
            if vis_bottom > top {
                let cy = (top + vis_bottom) * 0.5;
                let v = ((bottom - cy) / area_h).clamp(0.0, 1.0);
                let a = (0.65 + 0.35 * v) * alpha;
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, vis_bottom)),
                    0.0,
                    color.gamma_multiply(a),
                );
            }
            cur -= block_h;
        }

        // Peak box: a lit segment hanging at the peak level, snapped
        // to the grid so it jumps cell-to-cell like a real VFD
        // marker — the `peaks` decay drives it.
        if peak > 0.001 {
            let k = (peak * area_h / block_h).ceil().max(1.0);
            let peak_bottom = (bottom - (k - 1.0) * block_h - gap).min(bottom);
            let peak_top = (peak_bottom - slab_h).max(top_y);
            if peak_bottom > peak_top {
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x0, peak_top), Pos2::new(x1, peak_bottom)),
                    0.0,
                    color.gamma_multiply(0.95 * alpha),
                );
            }
        }
    }

    /// The mini drawer's variant: identical geometry to `paint` —
    /// the same bar/slab metrics the normal strip produces — but the
    /// narrow strip fits only as many of those full-size columns as
    /// the pitch allows and sub-samples `displayed` bins across them.
    /// `norm_w` is the normal window's virtual width and `s` the UI
    /// scale, so `sx`/`sy` resolve exactly like the normal view's.
    pub fn paint_mini(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        cfg: &settings::Spectrum,
        colors: &[Color32],
        alpha: f32,
        s: f32,
        norm_w: f32,
    ) {
        if alpha <= 0.001 || colors.is_empty() {
            return;
        }
        let total = self.displayed.len();
        if total == 0 {
            return;
        }
        let bottom = rect.max.y.round();
        let area_h = rect.height();
        if area_h <= 4.0 {
            return;
        }
        let denom = self.global_max * 1.1 + 1e-9;
        // Same formulas as `paint`, evaluated at the normal strip's
        // scale — bar width, spacing, and segment pitch render at the
        // same physical sizes there; only the column count differs.
        let sx = norm_w / RENDER_W;
        let sy = (cfg.height * s) / RENDER_H;
        let norm_bar_w = (cfg.bar_width * BAR_W_SCALE * sx).round().max(1.0);
        let norm_spacing = (cfg.bar_spacing * sx).round().max(2.0);
        let fit = ((rect.width() + norm_spacing) / (norm_bar_w + norm_spacing))
            .floor()
            .max(1.0) as usize;
        // ~2.1x the normal-fit column count: bars and gaps shrink
        // proportionally so the strip keeps the normal look — the
        // columns are just a bit narrower, not differently shaped.
        let n = ((fit as f32 * 2.1).round() as usize).clamp(1, total.max(1));
        let k = (fit as f32 / n as f32).min(1.0);
        let bar_w = (norm_bar_w * k).round().max(1.0);
        let spacing = (norm_spacing * k).round().max(1.0);
        let pitch = bar_w + spacing;
        let grid_w = n as f32 * pitch - spacing;
        let x0_base = (rect.min.x + (rect.width() - grid_w).max(0.0) * 0.5).round();
        let block_h = (BLOCK_H * sy).round().max(3.0);
        let gap = (block_h * 0.35).round().max(1.0);
        let slab_h = ((block_h - gap) * 0.5).round().max(1.0);
        for i in 0..n {
            let src = i * total / n;
            let x0 = x0_base + i as f32 * pitch;
            let x1 = (x0 + bar_w).min(rect.max.x);
            if x1 <= x0 {
                break;
            }
            let level = (self.displayed[src] / denom).clamp(0.0, 1.0);
            let peak = (self.peaks[src] / denom).clamp(0.0, 1.0);
            let color = gradient_color(colors, i, n);
            self.paint_column(
                painter,
                x0,
                x1,
                rect.min.y,
                bottom,
                area_h,
                level,
                peak,
                color,
                alpha,
                block_h,
                gap,
                slab_h,
            );
        }
    }
}

/// Horizontal gradient across bar index (GetGradientColor): `colors`
/// as evenly-spaced stops lerped by `u = i / (count - 1)`.
fn gradient_color(colors: &[Color32], i: usize, count: usize) -> Color32 {
    if colors.len() == 1 {
        return colors[0];
    }
    let u = if count <= 1 { 0.0 } else { i as f32 / (count - 1) as f32 };
    let span = (colors.len() - 1) as f32;
    let pos = (u * span).min(span);
    let idx = (pos as usize).min(colors.len() - 2);
    let t = pos - idx as f32;
    let (a, b) = (colors[idx], colors[idx + 1]);
    Color32::from_rgb(
        (a.r() as f32 + (b.r() as f32 - a.r() as f32) * t) as u8,
        (a.g() as f32 + (b.g() as f32 - a.g() as f32) * t) as u8,
        (a.b() as f32 + (b.b() as f32 - a.b() as f32) * t) as u8,
    )
}

/// Port of `BitmapColorHelper.PickDistinctVividColors`: up to `max`
/// hue-distinct, vivid colors from a cover image (32x32 downsample,
/// 16-quantized bins scored by count x chroma^2, then hue-distinct
/// picks overdriven for pop). Used by `UseMusicSpectrumCoverColors`.
pub fn dominant_colors(img: &egui::ColorImage, max: usize) -> Vec<Color32> {
    if max == 0 {
        return Vec::new();
    }
    // Downsample to 32x32 like TrySampleBitmapPixels.
    let (w, h) = (img.width() as u32, img.height() as u32);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let rgba = image::RgbaImage::from_raw(w, h, img.as_raw().to_vec());
    let Some(rgba) = rgba else { return Vec::new() };
    let small = image::imageops::resize(&rgba, 32, 32, image::imageops::FilterType::Triangle);

    // Bin colors (channel / 16) after dropping transparent + near-black
    // pixels; count and keep channel sums for an unbiased average.
    let mut counts: HashMap<u32, (u32, u32, u32, u32)> = HashMap::new();
    let mut saw_opaque = false;
    let mut saw_non_black = false;
    for px in small.pixels() {
        let [r, g, b, a] = px.0;
        if a >= 128 {
            saw_opaque = true;
        }
        if !(r < 20 && g < 20 && b < 20) {
            saw_non_black = true;
        }
        if a < 32 || (r < 20 && g < 20 && b < 20) {
            continue;
        }
        let key = ((r as u32 / 16 * 16) << 16) | ((g as u32 / 16 * 16) << 8) | (b as u32 / 16 * 16);
        let e = counts.entry(key).or_insert((0, 0, 0, 0));
        e.0 += 1;
        e.1 += r as u32;
        e.2 += g as u32;
        e.3 += b as u32;
    }
    if counts.is_empty() {
        // Second pass with minimal filtering (exact color keys, a > 16).
        for px in small.pixels() {
            let [r, g, b, a] = px.0;
            if a <= 16 {
                continue;
            }
            let key = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
            let e = counts.entry(key).or_insert((0, 0, 0, 0));
            e.0 += 1;
            e.1 += r as u32;
            e.2 += g as u32;
            e.3 += b as u32;
        }
    }

    // Score = frequency x chroma^2 so grays lose to vivid colors.
    let mut scored: Vec<(Color32, f32)> = counts
        .values()
        .map(|&(count, sr, sg, sb)| {
            let c = Color32::from_rgb(
                (sr / count) as u8,
                (sg / count) as u8,
                (sb / count) as u8,
            );
            let (mx, mn) = (c.r().max(c.g()).max(c.b()), c.r().min(c.g()).min(c.b()));
            let chroma = (mx - mn) as f32 / 255.0;
            (c, count as f32 * chroma * chroma)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if scored.is_empty() || (!saw_opaque && !saw_non_black) {
        return Vec::new();
    }

    let mut picks: Vec<Color32> = vec![overdrive(scored[0].0)];
    let primary_hue = hue(picks[0]);
    // Hue-distinct picks first (diff > 0.6 on the 0..6 hue scale, and
    // not within 0.35 of an existing pick).
    for &(c, _) in scored.iter().skip(1) {
        if picks.len() >= max {
            break;
        }
        let cand = overdrive(c);
        let h = hue(cand);
        let mut diff = (h - primary_hue).abs();
        if diff > 3.0 {
            diff = 6.0 - diff;
        }
        if diff > 0.6 && !picks.iter().any(|pc| (hue(*pc) - h).abs() < 0.35) {
            picks.push(cand);
        }
    }
    // Then any remaining distinct colors, hue-distinctness relaxed.
    for &(c, _) in scored.iter().skip(1) {
        if picks.len() >= max {
            break;
        }
        let cand = overdrive(c);
        if !picks.contains(&cand) {
            picks.push(cand);
        }
    }
    // Still short: synthesize by hue-shifting the primary.
    let step = if max <= 1 { 0.0 } else { 2.0 / max as f32 };
    while picks.len() < max {
        picks.push(shift_hue(picks[0], step * picks.len() as f32));
    }
    picks
}

/// `OverdriveColor`: boost saturation/brightness up to 20%.
fn overdrive(c: Color32) -> Color32 {
    let (r, g, b) = (c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
    let max = r.max(g).max(b);
    if max <= 0.0 {
        return c;
    }
    let f = (1.0 / max).min(1.2);
    Color32::from_rgb(
        (r * f * 255.0).clamp(0.0, 255.0) as u8,
        (g * f * 255.0).clamp(0.0, 255.0) as u8,
        (b * f * 255.0).clamp(0.0, 255.0) as u8,
    )
}

/// `GetHue` on a 0..6 scale. Note: the C# helper's equality guards use
/// `< 0` so every color resolves through the last branch — replicated
/// here (identical picks depend on it).
fn hue(c: Color32) -> f32 {
    let (r, g, b) = (c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d == 0.0 {
        return 0.0;
    }
    let h = 4.0 + (r - g) / d;
    if h < 0.0 { h + 6.0 } else { h }
}

/// `ShiftHue`: rotate hue by `delta` on the 0..6 scale, then overdrive.
fn shift_hue(c: Color32, delta: f32) -> Color32 {
    let (r, g, b) = (c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d < 0.001 {
        return overdrive(Color32::from_rgb(
            c.r().wrapping_add(48),
            c.g().wrapping_add(24),
            c.b(),
        ));
    }
    let mut h = hue(c) + delta;
    while h >= 6.0 {
        h -= 6.0;
    }
    while h < 0.0 {
        h += 6.0;
    }
    let s = d / max;
    let v = max;
    let cv = v * s;
    let x = cv * (1.0 - (h % 2.0 - 1.0).abs());
    let m = v - cv;
    let (rf, gf, bf) = if h < 1.0 {
        (cv, x, 0.0)
    } else if h < 2.0 {
        (x, cv, 0.0)
    } else if h < 3.0 {
        (0.0, cv, x)
    } else if h < 4.0 {
        (0.0, x, cv)
    } else if h < 5.0 {
        (x, 0.0, cv)
    } else {
        (cv, 0.0, x)
    };
    overdrive(Color32::from_rgb(
        ((rf + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((gf + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((bf + m) * 255.0).clamp(0.0, 255.0) as u8,
    ))
}
