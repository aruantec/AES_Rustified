//! Pillarbox detection — a lean port of Lacrima's
//! `PillarboxBarDetector` (Standard profile, side bars only — the
//! capture pipeline discards the vertical insets there too).
//!
//! A frame column counts as a "uniform bar" when ≥94% of 17 sampled
//! rows are near-black (max channel ≤ 12), at most one sampled row
//! holds real content (≥22), and the column's mean luma stays ≤ 8.
//! The scan walks inward from each edge, is capped at 22% of the
//! width per side, requires ≥8 px bars, a substantial central image
//! (so a loading screen never crops), and a ≥97%-black region check
//! per side before accepting an inset.

const BORDER_MAX: u8 = 12;
const CONTENT_MIN: u8 = 22;
const MIN_BAR_PX: usize = 8;

/// Lacrima's PillarboxDetectionProfile — only `Aggressive` survives
/// here: `AggressivePillarboxCrop` is `RemoveArcadePillarboxBars`
/// itself, so detection always runs with the arcade-tolerant
/// ceilings whenever the toggle is on.
#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    Aggressive,
}

/// Left/right black-bar insets in pixels for an RGBA frame.
/// `(0, 0)` when no uniform bars are present.
pub fn detect(rgba: &[u8], w: usize, h: usize, profile: Profile) -> (u32, u32) {
    if w < 80 || h < 80 || rgba.len() < w * h * 4 {
        return (0, 0);
    }
    let (min_content_w, max_side) = match profile {
        Profile::Aggressive => (0.24, 0.40),
    };
    let rows = sample_lines(h);
    let cols = sample_lines(w);
    if !central_content(rgba, w, h, &rows, &cols) {
        return (0, 0);
    }
    let max_scan = 24.max((w as f32 * max_side) as usize).min(w / 2);
    let mut left = scan(rgba, w, h, &rows, max_scan, false);
    let mut right = scan(rgba, w, h, &rows, max_scan, true);
    if left < MIN_BAR_PX {
        left = 0;
    }
    if right < MIN_BAR_PX {
        right = 0;
    }
    let cap = MIN_BAR_PX.max((w as f32 * max_side) as usize);
    left = left.min(cap);
    right = right.min(cap);
    let content_w = w.saturating_sub(left + right);
    if content_w < (w as f32 * min_content_w) as usize {
        return (0, 0);
    }
    // The bar must be uniformly black across its FULL height —
    // sample-column consensus alone can clip a scene whose edge rows
    // are dark.
    if left > 0 && !uniform_black_region(rgba, w, h, 0, left) {
        left = 0;
    }
    if right > 0 && !uniform_black_region(rgba, w, h, w - right, w) {
        right = 0;
    }
    if left + right > 0 && w - left - right < (w as f32 * min_content_w) as usize {
        return (0, 0);
    }
    (left as u32, right as u32)
}

/// Lacrima's ArcadePillarboxCropResolver::Resolve — detected insets
/// get a safety margin pulled back, the crop is symmetricized, then
/// each side keeps half of whatever the symmetric crop under-removes
/// vs the detected bar (VisiblePillarScale = 0.5).
pub fn resolve(frame_w: u32, det: (u32, u32)) -> (u32, u32) {
    if frame_w == 0 {
        return (0, 0);
    }
    let margin = 12.max(frame_w / 90);
    let l = det.0.saturating_sub(margin);
    let r = det.1.saturating_sub(margin);
    let content = frame_w.saturating_sub(l + r);
    if content == 0 {
        return (0, 0);
    }
    let total = frame_w - content.clamp(1, frame_w - 1);
    let (sl, sr) = (total / 2, total - total / 2);
    (blend(det.0, sl), blend(det.1, sr))
}

/// RetainPartialPillars — when the detected bar is deeper than the
/// resolved inset, keep half of the difference visible.
fn blend(detected: u32, resolved: u32) -> u32 {
    if detected == 0 || detected <= resolved {
        return resolved;
    }
    detected - (detected - resolved + 1) / 2
}

/// A locked crop stored at `stored_fw` px re-scaled to the live
/// frame width (Lacrima's TryGetLockedCrop / ScaleCrop).
pub fn scale_locked(left: u32, right: u32, stored_fw: u32, fw: u32) -> (u32, u32) {
    if stored_fw == 0 {
        return (left, right);
    }
    let s = fw as f32 / stored_fw as f32;
    (
        (left as f32 * s).round() as u32,
        (right as f32 * s).round() as u32,
    )
}

/// 17 evenly spaced sample indices over `extent` (Lacrima's
/// BuildSampleLines — centers of 1/17 segments).
fn sample_lines(extent: usize) -> [usize; 17] {
    let mut out = [0usize; 17];
    for (i, l) in out.iter_mut().enumerate() {
        *l = (((i as f32 + 0.5) / 17.0) * (extent - 1) as f32) as usize;
    }
    out
}

#[inline]
fn channel_max(rgba: &[u8], off: usize) -> u8 {
    rgba[off].max(rgba[off + 1]).max(rgba[off + 2])
}

/// A frame needs a bright central region before bars mean anything
/// — otherwise every dark scene would read as content-free.
fn central_content(rgba: &[u8], w: usize, h: usize, rows: &[usize; 17], cols: &[usize; 17]) -> bool {
    let (il, ir, it, ib) = (w / 5, w * 4 / 5, h / 5, h * 4 / 5);
    let mut sampled = 0usize;
    let mut content = 0usize;
    for &y in rows {
        if !(it..=ib).contains(&y) {
            continue;
        }
        for &x in cols {
            if !(il..=ir).contains(&x) {
                continue;
            }
            sampled += 1;
            if channel_max(rgba, y * w * 4 + x * 4) >= CONTENT_MIN {
                content += 1;
            }
        }
    }
    sampled > 0 && content >= 12.max(sampled * 18 / 100)
}

/// Inset of uniform bar columns walking in from one edge; `right`
/// flips the direction.
fn scan(rgba: &[u8], w: usize, h: usize, rows: &[usize; 17], max: usize, right: bool) -> usize {
    for i in 0..max.min(w) {
        let x = if right { w - 1 - i } else { i };
        if !is_bar_column(rgba, w, h, rows, x) {
            return i.saturating_sub(1);
        }
    }
    0
}

fn is_bar_column(rgba: &[u8], w: usize, h: usize, rows: &[usize; 17], x: usize) -> bool {
    let mut black = 0usize;
    let mut content = 0usize;
    let mut luma_sum = 0usize;
    let mut samples = 0usize;
    for &y in rows {
        if y >= h {
            continue;
        }
        let luma = channel_max(rgba, y * w * 4 + x * 4) as usize;
        luma_sum += luma;
        samples += 1;
        if luma <= BORDER_MAX as usize {
            black += 1;
        } else if luma >= CONTENT_MIN as usize {
            content += 1;
        }
    }
    if samples == 0 {
        return true;
    }
    content <= 1 && luma_sum / samples <= 8 && black >= (samples * 94).div_ceil(100)
}

/// ≥97% of a coarse 8x8-ish sample grid inside [x0,x1)×[0,h) is
/// near-black.
fn uniform_black_region(rgba: &[u8], w: usize, h: usize, x0: usize, x1: usize) -> bool {
    if x1 <= x0 {
        return false;
    }
    let step_x = ((x1 - x0) / 8).max(1);
    let step_y = (h / 8).max(1);
    let mut samples = 0usize;
    let mut black = 0usize;
    let mut y = 0;
    while y < h {
        let mut x = x0;
        while x < x1 {
            samples += 1;
            if channel_max(rgba, y * w * 4 + x * 4) <= BORDER_MAX {
                black += 1;
            }
            x += step_x;
        }
        y += step_y;
    }
    samples > 0 && black >= (samples * 97).div_ceil(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, left: usize, right: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                if x >= left && x < w - right {
                    let o = (y * w + x) * 4;
                    px[o] = 200;
                    px[o + 1] = 120;
                    px[o + 2] = 80;
                    px[o + 3] = 255;
                }
            }
        }
        px
    }

    #[test]
    fn detects_side_bars() {
        // 1280x720 with 160px black pillars each side (~4:3 inside 16:9).
        assert_eq!(
            detect(&frame(1280, 720, 160, 160), 1280, 720, Profile::Aggressive),
            (159, 159)
        );
    }

    #[test]
    fn ignores_native_widescreen() {
        // Full-width bright frame — nothing to crop.
        assert_eq!(
            detect(&frame(1280, 720, 0, 0), 1280, 720, Profile::Aggressive),
            (0, 0)
        );
    }

    #[test]
    fn ignores_all_black_frame() {
        // Loading screen — no central content means no crop.
        assert_eq!(
            detect(&vec![0u8; 1280 * 720 * 4], 1280, 720, Profile::Aggressive),
            (0, 0)
        );
    }

    #[test]
    fn resolve_margins_and_symmetrizes() {
        // Detected 200/160 on a 1280 frame: margin w/90=14 pulls each
        // back, then the crop splits symmetric and retains half the
        // per-side excess (Lacrima's resolver math).
        let (l, r) = resolve(1280, (200, 160));
        // margin=14 -> 186/146; content=948 -> symmetric 166/166;
        // left keeps half its excess (200-166)/2 -> 183, right 166.
        assert_eq!((l, r), (183, 166));
    }

    #[test]
    fn locked_crop_scales() {
        assert_eq!(scale_locked(160, 160, 1280, 1920), (240, 240));
    }

    #[test]
    fn ignores_dim_scene_edge() {
        // A dark-but-not-black scene edge (luma ~18, above the 12
        // border threshold) is content, not a bar — nothing crops.
        let mut px = frame(1280, 720, 160, 160);
        for y in 0..720 {
            for x in 0..160 {
                let o = (y * 1280 + x) * 4;
                px[o..o + 4].copy_from_slice(&[18, 18, 18, 255]);
            }
        }
        let (l, _) = detect(&px, 1280, 720, Profile::Aggressive);
        assert_eq!(l, 0);
    }
}
