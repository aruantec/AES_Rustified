//! Color emoji rendering. egui's glyph atlas is alpha-only, so
//! seguiemj's symbols draw monochrome while Lacrima (DirectWrite)
//! gets them colored. seguiemj ships COLRv1 paints on Windows 11 and
//! CBDT PNG strikes on Windows 10 — skrifa exposes both, rasterized
//! into cached egui textures here and drawn inline by `rich_text`.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Vec2};
use read_fonts::types::BoundingBox;
use skrifa::bitmap::BitmapData;
use skrifa::color::{Brush, ColorPainter, CompositeMode, Extend};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use std::sync::{Arc, OnceLock};
use tiny_skia as ts;

/// Emoji texture raster size — big enough to stay crisp at the
/// playlist's ~12px and the bar title's ~30px.
const TEX: u32 = 128;

/// Broad emoji-ish codepoint check — chars without a color strike in
/// seguiemj just fall back to the glyph path, so over-matching is
/// safe (VS16/ZWJ glue chars aren't matched and stay in text runs).
fn is_emoji(c: char) -> bool {
    let u = c as u32;
    (0x1F000..=0x1FAFF).contains(&u)
        || (0x2600..=0x27BF).contains(&u)
        || (0x2B00..=0x2BFF).contains(&u)
        || (0x2300..=0x23FF).contains(&u)
        || (0x2190..=0x21FF).contains(&u)
        || matches!(u, 0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139)
}

/// seguiemj's FontData — the same Arc egui rasterizes its glyph
/// fallback chain with, handed over by the font-loader so this module
/// doesn't keep its own copy of the ~10MB font.
static FONT_DATA: OnceLock<Arc<egui::FontData>> = OnceLock::new();

/// Install seguiemj's FontData (from `app_font_definitions`) — must
/// run before the first emoji render; idempotent.
pub(crate) fn set_font_data(fd: Arc<egui::FontData>) {
    let _ = FONT_DATA.set(fd);
}

/// The 'static font bytes + face index for `font()`: a leaked
/// reference into the Arc'd FontData (a process-lifetime singleton),
/// or the fs::read fallback when egui's copy didn't load.
fn font_bytes() -> Option<(&'static [u8], u32)> {
    static BYTES: OnceLock<Option<(&'static [u8], u32)>> = OnceLock::new();
    static READ: OnceLock<Vec<u8>> = OnceLock::new();
    *BYTES.get_or_init(|| {
        if let Some(fd) = FONT_DATA.get() {
            let fd: &'static egui::FontData = unsafe { &*Arc::into_raw(fd.clone()) };
            Some((fd.font.as_ref().as_ref(), fd.index))
        } else {
            let v = READ.get_or_init(|| {
                std::fs::read(r"C:\Windows\Fonts\seguiemj.ttf").unwrap_or_default()
            });
            (!v.is_empty()).then_some((v.as_slice(), 0))
        }
    })
}

/// seguiemj's face — table dir parsed once per render (it's cheap).
fn font() -> Option<FontRef<'static>> {
    let (bytes, index) = font_bytes()?;
    skrifa::FontRef::from_index(bytes, index).ok()
}

// --- COLR painting ----------------------------------------------------

/// a ∘ b — apply `b` first (tiny-skia transforms are row-major,
/// p' = [sx kx tx; ky sy ty] · p).
fn xf_mul(a: ts::Transform, b: ts::Transform) -> ts::Transform {
    ts::Transform::from_row(
        a.sx * b.sx + a.kx * b.ky,
        a.ky * b.sx + a.sy * b.ky,
        a.sx * b.kx + a.kx * b.sy,
        a.ky * b.kx + a.sy * b.sy,
        a.sx * b.tx + a.kx * b.ty + a.tx,
        a.ky * b.tx + a.sy * b.ty + a.ty,
    )
}

fn to_ts(t: skrifa::color::Transform) -> ts::Transform {
    ts::Transform::from_row(t.xx, t.yx, t.xy, t.yy, t.dx, t.dy)
}

/// OutlinePen → tiny-skia PathBuilder, in font units.
struct PathPen(ts::PathBuilder);

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, cy0, x, y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// COLR ColorPainter → tiny-skia. `fill` fills the current clip;
/// clips are ClipMask intersections, composite modes are layer blends.
struct ColrPainter<'a> {
    font: FontRef<'a>,
    layers: Vec<ts::Pixmap>,
    modes: Vec<ts::BlendMode>,
    xf: ts::Transform,
    xf_stack: Vec<ts::Transform>,
    clips: Vec<ts::Mask>,
    palette: Vec<ts::Color>,
}

impl<'a> ColrPainter<'a> {
    fn new(font: FontRef<'a>, size: u32) -> Self {
        let upem = font
            .metrics(Size::unscaled(), LocationRef::default())
            .units_per_em;
        let k = size as f32 / upem as f32;
        let palette = font
            .color_palettes()
            .get(0)
            .map(|p| {
                p.colors()
                    .iter()
                    .map(|c| ts::Color::from_rgba8(c.red(), c.green(), c.blue(), c.alpha()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            font,
            layers: vec![ts::Pixmap::new(size, size).unwrap()],
            modes: Vec::new(),
            xf: ts::Transform::from_row(k, 0.0, 0.0, -k, 0.0, size as f32),
            xf_stack: Vec::new(),
            clips: Vec::new(),
            palette,
        }
    }

    fn glyph_path(&self, gid: GlyphId) -> Option<ts::Path> {
        let glyph = self.font.outline_glyphs().get(gid)?;
        let mut pen = PathPen(ts::PathBuilder::new());
        glyph
            .draw(
                DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                &mut pen,
            )
            .ok()?;
        pen.0.finish()
    }

    fn pal_color(&self, index: u16, alpha: f32) -> ts::Color {
        match self.palette.get(index as usize) {
            Some(c) => ts::Color::from_rgba(
                c.red(),
                c.green(),
                c.blue(),
                (c.alpha() * alpha).clamp(0.0, 1.0),
            )
            .unwrap_or(ts::Color::TRANSPARENT),
            None => ts::Color::TRANSPARENT,
        }
    }

    fn stops(&self, color_stops: &[skrifa::color::ColorStop]) -> Vec<ts::GradientStop> {
        color_stops
            .iter()
            .map(|s| ts::GradientStop::new(s.offset, self.pal_color(s.palette_index, s.alpha)))
            .collect()
    }

    fn spread(e: Extend) -> ts::SpreadMode {
        match e {
            Extend::Repeat => ts::SpreadMode::Repeat,
            Extend::Reflect => ts::SpreadMode::Reflect,
            _ => ts::SpreadMode::Pad,
        }
    }

    fn paint_for(&self, brush: &Brush) -> ts::Paint<'static> {
        let shader = match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => ts::Shader::SolidColor(self.pal_color(*palette_index, *alpha)),
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => ts::LinearGradient::new(
                ts::Point::from_xy(p0.x, p0.y),
                ts::Point::from_xy(p1.x, p1.y),
                self.stops(color_stops),
                Self::spread(*extend),
                self.xf,
            )
            .unwrap_or(ts::Shader::SolidColor(ts::Color::TRANSPARENT)),
            Brush::RadialGradient {
                c0,
                c1,
                r1,
                color_stops,
                extend,
                ..
            } => ts::RadialGradient::new(
                ts::Point::from_xy(c0.x, c0.y),
                ts::Point::from_xy(c1.x, c1.y),
                *r1,
                self.stops(color_stops),
                Self::spread(*extend),
                self.xf,
            )
            .unwrap_or(ts::Shader::SolidColor(ts::Color::TRANSPARENT)),
            // tiny-skia has no sweep shader — rare in seguiemj; a solid
            // mid-stop color keeps the shape legible.
            Brush::SweepGradient { color_stops, .. } => ts::Shader::SolidColor(
                color_stops
                    .get(color_stops.len() / 2)
                    .map(|s| self.pal_color(s.palette_index, s.alpha))
                    .unwrap_or(ts::Color::TRANSPARENT),
            ),
        };
        ts::Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        }
    }

    fn target(&mut self) -> &mut ts::Pixmap {
        self.layers.last_mut().unwrap()
    }

    fn finish(mut self) -> Option<egui::ColorImage> {
        let px = self.layers.pop()?;
        // tiny-skia is premultiplied — egui wants straight RGBA.
        let mut data = px.data().to_vec();
        for px in data.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                px[0] = (px[0] as u32 * 255 / a).min(255) as u8;
                px[1] = (px[1] as u32 * 255 / a).min(255) as u8;
                px[2] = (px[2] as u32 * 255 / a).min(255) as u8;
            }
        }
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [TEX as usize, TEX as usize],
            &data,
        ))
    }
}

fn blend(mode: CompositeMode) -> ts::BlendMode {
    match mode {
        CompositeMode::Clear => ts::BlendMode::Clear,
        CompositeMode::Src => ts::BlendMode::Source,
        CompositeMode::Dest => ts::BlendMode::Destination,
        CompositeMode::SrcOver => ts::BlendMode::SourceOver,
        CompositeMode::DestOver => ts::BlendMode::DestinationOver,
        CompositeMode::SrcIn => ts::BlendMode::SourceIn,
        CompositeMode::DestIn => ts::BlendMode::DestinationIn,
        CompositeMode::SrcOut => ts::BlendMode::SourceOut,
        CompositeMode::DestOut => ts::BlendMode::DestinationOut,
        CompositeMode::SrcAtop => ts::BlendMode::SourceAtop,
        CompositeMode::DestAtop => ts::BlendMode::DestinationAtop,
        CompositeMode::Xor => ts::BlendMode::Xor,
        CompositeMode::Plus => ts::BlendMode::Plus,
        CompositeMode::Multiply => ts::BlendMode::Multiply,
        CompositeMode::Screen => ts::BlendMode::Screen,
        CompositeMode::Overlay => ts::BlendMode::Overlay,
        CompositeMode::Darken => ts::BlendMode::Darken,
        CompositeMode::Lighten => ts::BlendMode::Lighten,
        CompositeMode::ColorDodge => ts::BlendMode::ColorDodge,
        CompositeMode::ColorBurn => ts::BlendMode::ColorBurn,
        CompositeMode::HardLight => ts::BlendMode::HardLight,
        CompositeMode::SoftLight => ts::BlendMode::SoftLight,
        CompositeMode::Difference => ts::BlendMode::Difference,
        CompositeMode::Exclusion => ts::BlendMode::Exclusion,
        CompositeMode::HslHue => ts::BlendMode::Hue,
        CompositeMode::HslSaturation => ts::BlendMode::Saturation,
        CompositeMode::HslColor => ts::BlendMode::Color,
        CompositeMode::HslLuminosity => ts::BlendMode::Luminosity,
        _ => ts::BlendMode::SourceOver,
    }
}

impl ColorPainter for ColrPainter<'_> {
    fn push_transform(&mut self, t: skrifa::color::Transform) {
        self.xf_stack.push(self.xf);
        self.xf = xf_mul(self.xf, to_ts(t));
    }
    fn pop_transform(&mut self) {
        self.xf = self.xf_stack.pop().unwrap_or(self.xf);
    }
    fn push_clip_glyph(&mut self, gid: GlyphId) {
        let nested = !self.clips.is_empty();
        let mut mask = self
            .clips
            .last()
            .cloned()
            .unwrap_or_else(|| ts::Mask::new(TEX, TEX).unwrap());
        if let Some(path) = self.glyph_path(gid) {
            if nested {
                mask.intersect_path(&path, ts::FillRule::Winding, true, self.xf);
            } else {
                mask.fill_path(&path, ts::FillRule::Winding, true, self.xf);
            }
        }
        self.clips.push(mask);
    }
    fn push_clip_box(&mut self, clip_box: BoundingBox<f32>) {
        let nested = !self.clips.is_empty();
        let mut mask = self
            .clips
            .last()
            .cloned()
            .unwrap_or_else(|| ts::Mask::new(TEX, TEX).unwrap());
        if let Some(r) = ts::Rect::from_ltrb(
            clip_box.x_min,
            clip_box.y_min,
            clip_box.x_max,
            clip_box.y_max,
        ) {
            let p = ts::PathBuilder::from_rect(r);
            if nested {
                mask.intersect_path(&p, ts::FillRule::Winding, true, self.xf);
            } else {
                mask.fill_path(&p, ts::FillRule::Winding, true, self.xf);
            }
        }
        self.clips.push(mask);
    }
    fn pop_clip(&mut self) {
        self.clips.pop();
    }
    fn fill(&mut self, brush: Brush<'_>) {
        let paint = self.paint_for(&brush);
        let rect = ts::Rect::from_xywh(0.0, 0.0, TEX as f32, TEX as f32).unwrap();
        let mask = self.clips.last().cloned();
        self.layers
            .last_mut()
            .unwrap()
            .fill_rect(rect, &paint, ts::Transform::default(), mask.as_ref());
    }
    fn push_layer(&mut self, mode: CompositeMode) {
        self.layers.push(ts::Pixmap::new(TEX, TEX).unwrap());
        self.modes.push(blend(mode));
    }
    fn pop_layer_with_mode(&mut self, mode: CompositeMode) {
        if self.layers.len() <= 1 {
            return;
        }
        let layer = self.layers.pop().unwrap();
        let _ = self.modes.pop();
        self.target().draw_pixmap(
            0,
            0,
            layer.as_ref(),
            &ts::PixmapPaint {
                opacity: 1.0,
                blend_mode: blend(mode),
                quality: ts::FilterQuality::Bicubic,
            },
            ts::Transform::default(),
            None,
        );
    }
}

// --- raster + cache ----------------------------------------------------

/// Render `c`'s color image from seguiemj — COLR paint first
/// (resolution-independent, Win11), CBDT/sbix PNG strike as fallback
/// (Win10).
fn render_emoji(c: char) -> Option<egui::ColorImage> {
    let font = font()?;
    let gid = font.charmap().map(c)?;
    if gid == GlyphId::NOTDEF {
        return None;
    }
    if let Some(g) = font.color_glyphs().get(gid) {
        let mut p = ColrPainter::new(font.clone(), TEX);
        if g.paint(LocationRef::default(), &mut p).is_ok() {
            if let Some(img) = p.finish() {
                return Some(img);
            }
        }
    }
    if let Some(bm) = font
        .bitmap_strikes()
        .glyph_for_size(Size::new(TEX as f32), gid)
    {
        match bm.data {
            BitmapData::Png(png) => {
                let img = image::load_from_memory(png).ok()?.to_rgba8();
                let (w, h) = img.dimensions();
                return Some(egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &img,
                ));
            }
            BitmapData::Bgra(bgra) => {
                let rgba: Vec<u8> = bgra
                    .chunks_exact(4)
                    .flat_map(|p| {
                        let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
                        if a == 0 {
                            [0, 0, 0, 0]
                        } else {
                            [
                                (r * 255 / a).min(255) as u8,
                                (g * 255 / a).min(255) as u8,
                                (b * 255 / a).min(255) as u8,
                                a as u8,
                            ]
                        }
                    })
                    .collect();
                let (w, h) = (bm.width as usize, bm.height as usize);
                if rgba.len() == w * h * 4 {
                    return Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba));
                }
            }
            BitmapData::Mask(_) => {}
        }
    }
    None
}

/// `c`'s emoji texture — cached per char (`None` is cached too so a
/// non-color char doesn't re-parse the font every frame).
fn emoji_tex(ctx: &egui::Context, c: char) -> Option<egui::TextureHandle> {
    let id = egui::Id::unique(("emoji", c));
    if let Some(hit) = ctx.data_mut(|d| d.get_temp::<Option<egui::TextureHandle>>(id)) {
        return hit;
    }
    let tex = render_emoji(c).map(|img| {
        crate::shared_tex::load_tex(
            ctx,
            format!("emoji-{:x}", c as u32),
            img,
            egui::TextureOptions::LINEAR,
        )
    });
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// `(text, is_emoji)` segments of `text` — feed each into a LayoutJob
/// with emoji sections formatted `Color32::TRANSPARENT` so the glyphs
/// keep their advances and `galley_overlay` supplies the visible paint.
pub fn sections(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut run = String::new();
    for c in text.chars() {
        if is_emoji(c) {
            if !run.is_empty() {
                out.push((std::mem::take(&mut run), false));
            }
            out.push((c.to_string(), true));
        } else {
            run.push(c);
        }
    }
    if !run.is_empty() {
        out.push((run, false));
    }
    out
}

/// A LayoutJob for `text` where emoji chars sit in TRANSPARENT
/// sections — lays out and wraps identically to a plain job (same
/// advances); paint the galley, then `galley_overlay` draws the
/// color images on top of the invisible glyph spots.
pub fn rich_job(text: &str, format: egui::TextFormat) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    for (seg, emoji) in sections(text) {
        let mut f = format.clone();
        if emoji {
            f.color = Color32::TRANSPARENT;
        }
        job.append(&seg, 0.0, f);
    }
    job
}

/// The color-emoji overlay for a `rich_job` galley — paint after
/// `painter.galley` at the same `pos` (and with the same clip).
/// Glyph boxes come straight from the galley, so wrapping and
/// alignment need no second-guessing. `font_size` squares the
/// strikes like `rich_text`'s `font.size`; `tint` fades them with
/// the text (pass an alpha-multiplied white).
pub fn galley_overlay(
    painter: &egui::Painter,
    pos: Pos2,
    galley: &egui::Galley,
    font_size: f32,
    tint: Color32,
) {
    for row in &galley.rows {
        for g in &row.glyphs {
            if !is_emoji(g.chr) {
                continue;
            }
            let Some(tex) = emoji_tex(painter.ctx(), g.chr) else {
                continue;
            };
            // Baseline pos + advance → center the square strike in the
            // glyph's advance and line box.
            let c = Pos2::new(
                pos.x + row.pos.x + g.pos.x + g.advance_width * 0.5,
                pos.y + row.pos.y + g.pos.y - g.font_ascent + g.font_height * 0.5,
            );
            painter.image(
                tex.id(),
                Rect::from_center_size(c, Vec2::splat(font_size)),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                tint,
            );
        }
    }
}

/// The run's painted width as `rich_text` draws it — text runs plus
/// per-emoji glyph advances (needed to center the run).
fn run_width(painter: &egui::Painter, text: &str, font: &FontId) -> f32 {
    sections(text)
        .into_iter()
        .map(|(seg, _)| {
            painter
                .layout_no_wrap(seg, font.clone(), Color32::WHITE)
                .rect
                .width()
        })
        .sum()
}

/// `painter.text` LEFT_CENTER, but emoji chars draw as their color
/// bitmap/paint instead of the monochrome outline glyph.
pub fn rich_text(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font: &FontId,
    color: Color32,
) {
    if !text.chars().any(is_emoji) {
        painter.text(pos, Align2::LEFT_CENTER, text, font.clone(), color);
        return;
    }
    let ctx = painter.ctx().clone();
    let mut x = pos.x;
    let cy = pos.y;
    let mut run = String::new();
    let flush = |run: &mut String, x: &mut f32| {
        if run.is_empty() {
            return;
        }
        let g = painter.layout_no_wrap(std::mem::take(run), font.clone(), color);
        let w = g.rect.width();
        painter.add(egui::Shape::galley(
            Pos2::new(*x, cy - g.rect.height() * 0.5),
            g,
            color,
        ));
        *x += w;
    };
    for c in text.chars() {
        if is_emoji(c) {
            match emoji_tex(&ctx, c) {
                Some(tex) => {
                    flush(&mut run, &mut x);
                    // Advance = the outline glyph's advance; the
                    // square strike is centered inside it.
                    let adv = painter
                        .layout_no_wrap(c.to_string(), font.clone(), color)
                        .rect
                        .width();
                    painter.image(
                        tex.id(),
                        Rect::from_center_size(
                            Pos2::new(x + adv * 0.5, cy),
                            Vec2::splat(font.size),
                        ),
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE.gamma_multiply(color.a() as f32 / 255.0),
                    );
                    x += adv;
                }
                None => run.push(c),
            }
        } else {
            run.push(c);
        }
    }
    flush(&mut run, &mut x);
}

/// `painter.text` CENTER_CENTER with color emoji — the run's width is
/// measured (runs + glyph advances) so the whole line centers on `pos`.
pub fn rich_text_centered(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font: &FontId,
    color: Color32,
) {
    if !text.chars().any(is_emoji) {
        painter.text(pos, Align2::CENTER_CENTER, text, font.clone(), color);
        return;
    }
    let w = run_width(painter, text, font);
    rich_text(painter, Pos2::new(pos.x - w * 0.5, pos.y), text, font, color);
}

/// `painter.text` LEFT_TOP with color emoji — `pos` is the text's
/// top edge (the stacked-lines convention elsewhere in the views).
pub fn rich_text_top(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font: &FontId,
    color: Color32,
) {
    if !text.chars().any(is_emoji) {
        painter.text(pos, Align2::LEFT_TOP, text, font.clone(), color);
        return;
    }
    let h = painter
        .layout_no_wrap(text.to_owned(), font.clone(), color)
        .rect
        .height();
    rich_text(painter, Pos2::new(pos.x, pos.y + h * 0.5), text, font, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_emoji_covers_symbols_but_not_text() {
        for c in ['❤', '♥', '🔥', '🎵', '⭐', '⏯', '✨'] {
            assert!(is_emoji(c), "{c:?} should classify as emoji");
        }
        // VS16 / ZWJ must stay in text runs so they glue to the emoji.
        for c in ['a', 'Z', '5', '-', '。', 'あ', '\u{FE0F}', '\u{200D}'] {
            assert!(!is_emoji(c), "{c:?} must stay in the text run");
        }
    }

    #[test]
    fn seguiemj_renders_colored_emoji() {
        if !std::path::Path::new(r"C:\Windows\Fonts\seguiemj.ttf").exists() {
            return; // font absent — nothing to render from
        }
        for c in ['🔥', '❤', '🎵'] {
            let img = render_emoji(c).unwrap_or_else(|| panic!("{c:?} color image"));
            // The strike must actually contain color — a monochrome
            // render would be a single channel's worth of gray.
            let colored = img
                .pixels
                .iter()
                .any(|p| (p.r() as i32 - p.g() as i32).abs() > 16
                    || (p.r() as i32 - p.b() as i32).abs() > 16);
            assert!(colored, "{c:?} rendered without color");
        }
    }
}
