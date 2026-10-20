//! The mini shell — Lacrima's MinView skin over the same data: the
//! loaded album's items, the live player, the waveform seek bar, the
//! spectrum and the cover decode pool — nothing here owns playback
//! state. The now-playing bar sizes everything as a fraction of its
//! own height, so the layout keeps the same proportions at any UI
//! scale or window size.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Vec2};
use eframe::egui::epaint::{Mesh, Shape, Vertex};

use crate::{
    album_row, audio, carousel, ellipsize, equalizer, fmt_time, settings, spectrum_bar,
    transport, waveform_bar,
};

/// The mini window's floor — below this the rows stop fitting.
pub(crate) const MIN_SIZE: Vec2 = Vec2::new(340.0, 420.0);
/// First-run size when no mini geometry was persisted yet.
pub(crate) const DEFAULT_SIZE: Vec2 = Vec2::new(420.0, 560.0);

/// Now-playing bar height — fixed physical px matching Lacrima's
/// ~90px top row (raw 92pt rendered ~115px on a high-DPI display;
/// 80 lands at ~95-100px, Lacrima's size). Deliberately NOT
/// `s`-scaled: the bar keeps this size at any UI scaling.
const BAR_H: f32 = 80.0;
/// The control cluster's base-unit scale: PLAY_BTN (100) renders at
/// 50% of the bar height — Lacrima's proportions.
const CTRL_K: f32 = 0.0052;

/// Playback intents the shell can't serve itself — the app's shared
/// action tail resolves them exactly like the Music view's outputs.
#[derive(Default)]
pub(crate) struct MiniOut {
    /// A track the user picked from the list, or a step-path result.
    pub play: Option<PathBuf>,
    /// A waveform scrub release, in seconds.
    pub seek: Option<f64>,
    /// Prev/Next from the media controls — resolved like the band's.
    pub step: transport::Step,
}

/// Paint + interact for one frame of the mini shell. `content` is the
/// area below the title bar; geometry follows the small window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn view(
    ui: &mut egui::Ui,
    content: Rect,
    interactive: bool,
    carousel: &mut carousel::Carousel,
    row: &mut album_row::AlbumRow,
    player: Option<&audio::AudioPlayer>,
    playing_track: Option<&Path>,
    _stream_loading: bool,
    _waveform: &[f32],
    _waveform_loading: bool,
    bar: &mut waveform_bar::WaveformBar,
    transport: &mut transport::Transport,
    spectrum: &spectrum_bar::SpectrumBar,
    spec_cfg: &mut settings::Spectrum,
    eq: &mut settings::Equalizer,
    grid_bg: Color32,
    cover_colors: Option<&[Color32]>,
    accent: Color32,
    s: f32,
    // The normal window's virtual width — the spectrum strip sizes
    // its bars off it so mini columns match the normal view's.
    norm_w: f32,
) -> MiniOut {
    let painter = ui.painter().clone();
    painter.rect_filled(content, 0, grid_bg);
    let ctx = ui.ctx().clone();
    let mut out = MiniOut::default();

    // The header gradient is ONE continuous wash from the window top
    // (behind the title bar — its fill is transparent in mini mode)
    // through the now-playing bar, fading over ~two bar heights into
    // the background. The scope painter is clipped to `content`, so
    // this uses a clip widened to the title zone.
    let title_h = crate::MINI_TITLE_BAR_HEIGHT * s;
    let header = Rect::from_min_max(
        Pos2::new(content.min.x, content.min.y - title_h),
        Pos2::new(content.max.x, content.min.y + 2.0 * BAR_H),
    );
    let hp = painter.clone().with_clip_rect(header);
    // The zone under the title bar otherwise shows the bare window
    // clear color — fill it with the content bg so the surface is
    // seamless top to bottom.
    hp.rect_filled(
        Rect::from_min_max(header.min, Pos2::new(content.max.x, content.min.y)),
        0,
        grid_bg,
    );
    // Cover palette wash when art is loaded; otherwise Lacrima's
    // default — a grayish band at the top that darkens downward.
    let (left, right) = match cover_colors.filter(|c| !c.is_empty()) {
        Some(cols) => (cols[0], *cols.get(1).unwrap_or(&cols[0])),
        None => (Color32::from_gray(0x60), Color32::from_gray(0x60)),
    };
    let fade = Color32::TRANSPARENT;
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(header.left_top(), left.gamma_multiply(0.5));
    mesh.colored_vertex(header.right_top(), right.gamma_multiply(0.5));
    mesh.colored_vertex(header.right_bottom(), fade);
    mesh.colored_vertex(header.left_bottom(), fade);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    hp.add(egui::Shape::mesh(mesh));

    // --- Now-playing bar --------------------------------------------
    // All metrics are fractions of the bar height `h` — cover fills
    // it edge-to-edge minus a small pad, controls keep the same
    // proportions, so the bar looks identical at every scale.
    let bar_rect = Rect::from_min_size(content.min, Vec2::new(content.width(), BAR_H));
    let h = bar_rect.height();
    // Lacrima insets the bar's content horizontally (~12px).
    let pad_x = 0.13 * h;
    let pad_y = 0.06 * h;
    let cy = bar_rect.center().y;

    // Media controls, right-packed at the bar's edge (the full
    // band's center cluster at bar-relative scale).
    let cs = h * CTRL_K;
    let ctrl_w = transport::MINI_CONTROLS_SPAN * cs;
    let ctrl_rect = Rect::from_min_max(
        Pos2::new(bar_rect.max.x - pad_x - ctrl_w, bar_rect.min.y),
        Pos2::new(bar_rect.max.x - pad_x, bar_rect.max.y),
    );
    out.step = transport.controls(ui, ctrl_rect, player, 1.0, interactive, cs);

    // The playing item's own art, else the shared album placeholder.
    // Its left inset is tighter than the controls' right one.
    let cover = h - 2.0 * pad_y;
    let cover_rect = Rect::from_center_size(
        Pos2::new(bar_rect.min.x + 0.08 * h + cover * 0.5, cy),
        Vec2::splat(cover),
    );
    // Lacrima fills the square slot with the cover's palette — a
    // single dominant color behind the art. No palette yet: a subtle
    // elevation so the slot still reads as a cover surface.
    let slot_fill = cover_colors
        .and_then(|c| c.first().copied())
        .unwrap_or_else(|| Color32::from_gray(34));
    painter.rect_filled(cover_rect, 0, slot_fill);
    let art = playing_track
        .and_then(|t| carousel.cover_for(t))
        .and_then(|src| row.cover_tex(&ctx, &src));
    match &art {
        Some(tex) => {
            // Uniform fit — preserve the art's aspect, centered in the
            // square slot (a 16:9 thumbnail letterboxes like Lacrima,
            // never stretches).
            let [tw, th] = tex.size();
            let aspect = tw as f32 / (th as f32).max(1.0);
            let fit = if aspect >= 1.0 {
                Vec2::new(cover, cover / aspect)
            } else {
                Vec2::new(cover * aspect, cover)
            };
            painter.image(
                tex.id(),
                Rect::from_center_size(cover_rect.center(), fit),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None => {
            // Lacrima's mini default is a bare note glyph on the
            // palette backdrop — not the textured placeholder tile.
            // Same geometry as library::placeholder_image's note,
            // authored on a 400x400 canvas.
            let g = cover * 0.55;
            let r = Rect::from_center_size(cover_rect.center(), Vec2::splat(g));
            let k = g / 400.0;
            let at = |u: f32, v: f32| Pos2::new(r.min.x + u * k, r.min.y + v * k);
            // Light glyph — visible on the dark/colored backdrop
            // (Lacrima's is dark only because its tile is light).
            let col = Color32::from_gray(120);
            // Stems.
            painter.rect_filled(Rect::from_min_max(at(165.0, 110.0), at(180.0, 280.0)), 0, col);
            painter.rect_filled(Rect::from_min_max(at(285.0, 90.0), at(300.0, 260.0)), 0, col);
            // Note heads (ellipses).
            painter.add(egui::Shape::ellipse_filled(at(140.0, 290.0), Vec2::new(40.0, 30.0) * k, col));
            painter.add(egui::Shape::ellipse_filled(at(260.0, 270.0), Vec2::new(40.0, 30.0) * k, col));
            // Beam.
            painter.add(egui::Shape::convex_polygon(
                vec![at(165.0, 110.0), at(300.0, 90.0), at(300.0, 140.0), at(165.0, 160.0)],
                col,
                egui::Stroke::NONE,
            ));
        }
    }

    // Text lane between the cover and the controls.
    let tx = cover_rect.max.x + 0.10 * h;
    let lane_w = (ctrl_rect.min.x - 0.10 * h - tx).max(0.0);

    // Player state, small caps, pinned to the cover's top.
    let state = match player.map(|p| p.state()) {
        Some(audio::PlayerState::Playing) => "PLAYING",
        Some(audio::PlayerState::Paused) => "PAUSED",
        Some(_) => "STOPPED",
        None => "",
    };
    if !state.is_empty() {
        painter.text(
            Pos2::new(tx, cover_rect.min.y),
            Align2::LEFT_TOP,
            state,
            FontId::proportional(0.13 * h),
            Color32::from_gray(160),
        );
    }

    // Title — the carousel's marquee (hold, then scroll) when it
    // overflows the lane, ellipsized otherwise.
    let title = playing_track
        .map(|t| carousel.display_for(t).0)
        .unwrap_or_default();
    if !title.is_empty() {
        let font = FontId::proportional(0.15 * h);
        let galley = painter.layout_no_wrap(title.clone(), font.clone(), Color32::WHITE);
        let tw = galley.rect.width();
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        // Lacrima stacks the title just below the state line, not at
        // the bar's center — roughly the cover's upper third.
        let title_y = cover_rect.min.y + 0.38 * cover;
        if let Some(off) = carousel.mini_marquee(&title, tw, lane_w, s, dt) {
            ctx.request_repaint();
            let lane = Rect::from_center_size(
                Pos2::new(tx + lane_w * 0.5, title_y),
                Vec2::new(lane_w, galley.rect.height() * 1.2),
            );
            painter.with_clip_rect(lane).galley(
                Pos2::new(lane.min.x + off, lane.center().y - galley.rect.height() * 0.5),
                galley,
                Color32::WHITE,
            );
        } else {
            crate::emoji::rich_text(
                &painter,
                &ctx,
                Pos2::new(tx, title_y),
                &ellipsize(&painter, &title, &font, lane_w),
                &font,
                Color32::WHITE,
            );
        }
    }

    // position / duration, pinned to the cover's bottom — the
    // player's when it has one, else the item's tag duration so a
    // stopped bar still shows "0:00 / len".
    let pos_s = player.map(|p| p.position().as_secs_f64()).unwrap_or(0.0);
    let dur_s = player
        .map(|p| p.duration().as_secs_f64())
        .filter(|d| *d > 0.0)
        .or_else(|| {
            playing_track.and_then(|t| {
                carousel
                    .item_paths()
                    .find(|(_, p, _)| *p == t)
                    .and_then(|(_, _, d)| d)
            })
        })
        .unwrap_or(0.0);
    painter.text(
        Pos2::new(tx, cover_rect.max.y),
        Align2::LEFT_BOTTOM,
        format!("{} / {}", fmt_time(pos_s), fmt_time(dur_s)),
        FontId::proportional(0.16 * h),
        Color32::from_gray(160),
    );

    // --- Seek track (Lacrima's MinView progress bar) --------------
    // A slim pill below the header row, inset like the bar content.
    let track = Rect::from_min_max(
        Pos2::new(content.min.x + pad_x, bar_rect.max.y + 0.12 * h),
        Pos2::new(content.max.x - pad_x, bar_rect.max.y + 0.12 * h + 0.132 * h),
    );
    let pos_s2 = player.map(|p| p.position().as_secs_f64()).unwrap_or(0.0);
    let dur_s2 = player
        .map(|p| p.duration().as_secs_f64())
        .filter(|d| *d > 0.0)
        .unwrap_or(dur_s);
    out.seek = bar.mini_ui(ui, track, pos_s2, dur_s2, interactive, "mv-seek", false);

    // --- Toggle strip (Lacrima row 2) -------------------------------
    // Visualizer / Equalizer / Mute — 20px toggles, left-aligned,
    // white when active, DimGray when not. The ExtensionArea is
    // single-slot: enabling one clears the other.
    let icon = 0.25 * h;
    let iy = track.max.y + 0.10 * h;
    let igap = 0.10 * h;
    let on = Color32::WHITE;
    let off = Color32::from_rgb(0x69, 0x69, 0x69); // DimGray
    let toggle = |i: usize, id: &str, tip: &str| {
        let r = Rect::from_min_size(
            Pos2::new(content.min.x + pad_x + (icon + igap) * i as f32, iy),
            Vec2::splat(icon),
        );
        let resp = if interactive {
            ui.interact(r, ui.id().with(id), egui::Sense::click())
        } else {
            ui.interact(r, ui.id().with(id), egui::Sense::hover())
        };
        if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        (resp.on_hover_text(tip), r)
    };

    // Visualizer: five rounded bars (140x120 canvas), peaks
    // 68/100/80/104/68 like Lacrima's glyph.
    let (resp, r) = toggle(0, "tm-vis", "Visualizer");
    if resp.clicked() {
        spec_cfg.mini_visualizer = !spec_cfg.mini_visualizer;
        if spec_cfg.mini_visualizer {
            spec_cfg.mini_equalizer = false;
        }
    }
    // All three glyphs are normalized to the volume ring's height
    // (`gh`), vertically centered on the slot.
    let gh = icon * 0.67;
    let col = if spec_cfg.mini_visualizer { on } else { off };
    // Tallest bar is 104u — scale it to `gh`, center the 140u canvas.
    let k = gh / 104.0;
    let ox = r.min.x + (icon - 140.0 * k) * 0.5;
    let oy = r.min.y + (icon - 104.0 * k) * 0.5;
    for (bx, by, bh) in [(16.0, 36.0, 68.0), (40.0, 4.0, 100.0), (64.0, 24.0, 80.0), (88.0, 0.0, 104.0), (112.0, 36.0, 68.0)] {
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(ox + bx * k, oy + by * k),
                Vec2::new(14.0 * k, bh * k),
            ),
            egui::CornerRadius::same((7.0 * k).round() as u8),
            col,
        );
    }

    // Equalizer: three vertical sliders with knob ellipses (140x120).
    let (resp, r) = toggle(1, "tm-eq", "Equalizer");
    if resp.clicked() {
        spec_cfg.mini_equalizer = !spec_cfg.mini_equalizer;
        if spec_cfg.mini_equalizer {
            spec_cfg.mini_visualizer = false;
        }
    }
    let col = if spec_cfg.mini_equalizer { on } else { off };
    // Slider extent is 110u (6..116) — scale it to `gh`, center.
    let k = gh / 110.0;
    let ex = r.min.x + (icon - 140.0 * k) * 0.5;
    let ey = r.min.y + (icon - 110.0 * k) * 0.5 - 6.0 * k;
    for (bx, by, bh) in [(18.0, 10.0, 100.0), (56.0, 6.0, 110.0), (96.0, 10.0, 100.0)] {
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(ex + bx * k, ey + by * k),
                Vec2::new(12.0 * k, bh * k),
            ),
            egui::CornerRadius::same((6.0 * k).round() as u8),
            col,
        );
    }
    for (kx, ky) in [(24.0, 52.0), (62.0, 76.0), (102.0, 52.0)] {
        painter.circle_filled(
            Pos2::new(ex + kx * k, ey + ky * k),
            18.0 * k,
            col,
        );
    }

    // Volume — the search-pill treatment: a bordered circle around
    // the speaker that expands rightward on hover into the pill
    // slider (Lacrima's flyout PillProgressSlider, HorizontalAlignment
    // grows the box while the icon stays anchored). Volume 0 or a
    // stashed cap-mute shows the crossed-out speaker; clicking the
    // circle toggles mute like the knob's cap.
    let (resp, r) = toggle(2, "tm-mute", "Mute");
    if resp.clicked() {
        transport.toggle_mute(player);
    }
    let vol = player.map(|p| p.volume()).unwrap_or(0.0);
    let muted = transport.muted() || vol <= 0.0;

    // Collapsed: a circle around the icon; expanded: grows rightward
    // over the free area, same 0.35s CubicEaseOut as the search pill.
    let ph = gh * 1.15;
    let pl = r.center().x - ph * 0.5;
    let slider_w = 1.1 * h;
    let w_max = ph + 6.0 + slider_w + 6.0;
    let t = crate::search_pill::ease_out_cubic(ctx.animate_bool_with_time(
        ui.id().with("mv-vol-pill"),
        transport.mini_vol_open,
        0.35,
    ));
    let w = ph + (w_max - ph) * t;
    let pill = Rect::from_min_max(
        Pos2::new(pl, r.center().y - ph * 0.5),
        Pos2::new(pl + w, r.center().y + ph * 0.5),
    );
    // Pointer-rect test like the search pill (not widget hover — the
    // slider inside would fight it), plus kept open mid-scrub.
    let in_pill = ctx
        .input(|i| i.pointer.latest_pos())
        .is_some_and(|p| pill.contains(p));
    transport.mini_vol_open =
        interactive && (in_pill || bar.mini_scrubbing(&ctx, "mv-vol"));
    if in_pill {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let rr = egui::CornerRadius::same((ph * 0.5).round() as u8);
    painter.rect_filled(
        pill,
        rr,
        Color32::from_black_alpha((0xAA as f32 * t) as u8),
    );
    painter.rect_stroke(
        pill,
        rr,
        egui::Stroke::new(0.9, Color32::from_white_alpha(0xB0)),
        egui::StrokeKind::Inside,
    );
    // Speaker centered in the circle zone — dims to the muted glyph
    // when the level hits 0.
    let ic = Rect::from_center_size(
        Pos2::new(pl + ph * 0.5, r.center().y),
        Vec2::splat(icon * 0.62),
    );
    speaker(&painter, ic, muted, if muted { off } else { on });

    // The slider appears with the box (Lacrima fades the flyout's
    // contents with the same transition).
    if t > 0.4 {
        let slider = Rect::from_min_max(
            Pos2::new(pl + ph + 6.0, r.center().y - 3.5),
            Pos2::new(pill.max.x - 6.0, r.center().y + 3.5),
        );
        if let Some(v) = bar.mini_ui(ui, slider, vol as f64, 1.0, interactive, "mv-vol", true) {
            if let Some(p) = player {
                p.set_volume(v as f32);
            }
            transport.unmute();
        }
    }

    // Extension drawer (Lacrima's MaxHeightToggleBehavior row): the
    // slot grows 0→full over 0.62s QuadraticEaseInOut and the content
    // is pinned to the slot's bottom behind a clip, so it slides out
    // like the audio view's EQ drawer instead of growing in place.
    // Both drawers share one bar height (VisualizerView 90px ≈ h).
    //
    // Switching toggles is SEQUENCED: the open drawer closes first,
    // then the new content swaps in and opens — `shown` tracks which
    // content the drawer is displaying and only changes once the
    // reveal bottoms out.
    let want: u8 = if spec_cfg.mini_equalizer {
        2
    } else if spec_cfg.mini_visualizer {
        1
    } else {
        0
    };
    let p_id = egui::Id::new("mv-ext-p");
    let shown_id = egui::Id::new("mv-ext-shown");
    let mut p: f32 = ctx.data_mut(|d| d.get_temp(p_id)).unwrap_or(0.0);
    let mut shown: u8 = ctx.data_mut(|d| d.get_temp(shown_id)).unwrap_or(0);
    // Content only swaps at p==0; a mismatched request drives it shut.
    let target = if want != shown || shown == 0 { 0.0 } else { 1.0 };
    let dt = ctx.input(|i| i.stable_dt).min(0.1);
    p = (p + (target - p).signum() * dt / 0.62).clamp(0.0, 1.0);
    if p != target {
        ctx.request_repaint();
    }
    if want != shown && p <= 0.001 {
        shown = want;
        ctx.request_repaint();
    }
    ctx.data_mut(|d| {
        d.insert_temp(p_id, p);
        d.insert_temp(shown_id, shown);
    });
    let ext_t = ease_in_out_quad(p);
    if ext_t > 0.001 && shown != 0 {
        let slot = Rect::from_min_max(
            Pos2::new(content.min.x, iy + icon + 0.10 * h),
            Pos2::new(
                content.max.x,
                (iy + icon + 0.10 * h + h * ext_t).min(content.max.y),
            ),
        );
        if shown == 2 {
            // Same drawer height as the spectrum — the panel's layout
            // scale is derived so PANEL_H lands on `h` exactly, still
            // bottom-anchored + clipped like the audio view's drawer.
            if equalizer::panel(
                ui,
                slot,
                &mut eq.bands,
                1.0,
                interactive && ext_t > 0.95,
                h / equalizer::PANEL_H,
                // The drawer tints with the playing cover's palette —
                // the accent color stands in when there's no art.
                Some(
                    cover_colors
                        .and_then(|c| c.first().copied())
                        .unwrap_or(accent),
                ),
            ) {
                if let Some(pl) = player {
                    for (i, &db) in eq.bands.iter().enumerate() {
                        pl.set_band_gain(i, db);
                    }
                }
            }
        } else {
            let spec_rect = Rect::from_min_max(
                Pos2::new(slot.min.x + pad_x, slot.max.y - h),
                Pos2::new(slot.max.x - pad_x, slot.max.y),
            );
            let grad = spec_cfg.gradient.map(|c| Color32::from_rgb(c[0], c[1], c[2]));
            let cols: &[Color32] = if spec_cfg.cover_colors {
                cover_colors.filter(|c| !c.is_empty()).unwrap_or(&grad)
            } else {
                &grad
            };
            spectrum.paint_mini(
                &painter.with_clip_rect(slot),
                spec_rect,
                spec_cfg,
                cols,
                1.0,
                s,
                norm_w,
            );
            ctx.request_repaint();
        }
    }

    // --- Playlist (Lacrima's CoverItems list, Grid.Row=4) -----------
    // Three columns: right-aligned index ("n."), "Title - Album"
    // ellipsized, duration right-aligned ("--:--" unresolved). Multi-
    // select with Ctrl/Shift, double-click plays, drag reorders the
    // selection block with a smooth glide + dashed drop-slot holes
    // (album_row's DrawPlaceholder). The list tracks the drawer slot
    // live and stops at the footer strip.
    let footer_h = 0.31 * h; // Lacrima's Height=28 in the 90px world
    let foot_top = content.max.y - footer_h;
    painter.hline(
        content.min.x..=content.max.x,
        foot_top,
        egui::Stroke::new(1.0, Color32::from_white_alpha(30)),
    );
    // Resize-grip dots — Lacrima's 24x18 canvas pinned bottom-right
    // (Margin 0,0,0,7), six 3px #BDBDBD dots in a corner triangle.
    let u = h / 90.0;
    let gx = content.max.x - 24.0 * u;
    let gy = content.max.y - 25.0 * u;
    let dot_r = 1.5 * u;
    for (dx, dy) in [
        (16.0, 3.0),
        (11.0, 9.0),
        (16.0, 9.0),
        (6.0, 15.0),
        (11.0, 15.0),
        (16.0, 15.0),
    ] {
        painter.circle_filled(
            Pos2::new(gx + dx * u + dot_r, gy + dy * u + dot_r),
            dot_r,
            Color32::from_rgb(0xBD, 0xBD, 0xBD),
        );
    }

    let list_top = iy + icon + 0.10 * h + h * ext_t + 0.06 * h;
    let list = Rect::from_min_max(
        Pos2::new(content.min.x + pad_x, list_top),
        Pos2::new(content.max.x - pad_x * 0.6, foot_top),
    );
    // Lacrima's Row=4 backdrop: the loaded cover stretched
    // UniformToFill across the whole playlist band at 0.4 opacity,
    // under an opacity mask that fades it in from transparent at
    // the top to full by the 80% mark (LinearGradient 50%,1% →
    // 50%,80% in the XAML). Painted as a uv-mesh so the fill-crop
    // and the fade live in the same vertices.
    if let Some(tex) = &art {
        let bg = Rect::from_min_max(
            Pos2::new(content.min.x, list_top),
            Pos2::new(content.max.x, foot_top),
        );
        if bg.width() > 0.0 && bg.height() > 0.0 {
            let (tw, th) = (tex.size()[0] as f32, tex.size()[1] as f32);
            let fill = (bg.width() / tw).max(bg.height() / th);
            let wr = bg.width() / fill / tw;
            let hr = bg.height() / fill / th;
            let u0 = (1.0 - wr) * 0.5;
            let v0 = (1.0 - hr) * 0.5;
            // Opacity-mask ramp: 0% alpha at the top edge, border
            // opacity from 80% height down — 0.2 (Lacrima's 0.4
            // toned down per user).
            let a = 51u8;
            let fy = bg.min.y + 0.8 * bg.height();
            let fv = v0 + 0.8 * hr;
            let mut mesh = Mesh::with_texture(tex.id());
            for (py, vy, pa) in [
                (bg.min.y, v0, 0u8),
                (fy, fv, a),
                (bg.max.y, v0 + hr, a),
            ] {
                for (px, ux) in [(bg.min.x, u0), (bg.max.x, u0 + wr)] {
                    mesh.vertices.push(Vertex {
                        pos: Pos2::new(px, py),
                        uv: Pos2::new(ux, vy),
                        color: Color32::from_white_alpha(pa),
                    });
                }
            }
            mesh.indices.extend_from_slice(&[0, 1, 3, 0, 3, 2, 2, 3, 5, 2, 5, 4]);
            painter.add(Shape::mesh(mesh));
        }
    }
    if list.height() > 0.15 * h {
        let font = FontId::proportional(0.12 * h);
        let row_h = font.size * 1.75;
        let idx_w = painter
            .layout_no_wrap(
                format!("{}.", carousel.len()),
                font.clone(),
                Color32::BLACK,
            )
            .rect
            .width()
            + 0.10 * h; // Lacrima's 6,0,6,0 index margin
        // Duration column = the widest duration text (SharedSizeGroup)
        // pinned at Lacrima's right margin — the title ellipsizes
        // into the space left over, never overlapping it.
        let dur_w = rows_dur_w(&painter, &font, carousel);
        let dur_right = 0.11 * h; // Lacrima's 10px right margin
        let col_gap = 0.09 * h; // the 8px spacer column
        let idle_fg = Color32::from_gray(210);
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let follow = 1.0 - (-dt * 18.0).exp();

        // List state — session-temp, keyed off this ui.
        let sel_id = ui.id().with("mv-sel");
        let anchor_id = ui.id().with("mv-sel-anchor");
        let drag_id = ui.id().with("mv-list-drag");
        let disp_id = ui.id().with("mv-list-disp");
        let pal_id = ui.id().with("mv-sel-pal");
        let palkey_id = ui.id().with("mv-sel-pal-key");
        let palcol_id = ui.id().with("mv-sel-pal-col");
        // Seconds into the current drag — drives the stack gather.
        let dragt_id = ui.id().with("mv-drag-t");

        let mut sel: HashSet<PathBuf> =
            ctx.data_mut(|d| d.get_temp(sel_id)).unwrap_or_default();
        let mut anchor: usize =
            ctx.data_mut(|d| d.get_temp(anchor_id)).unwrap_or(0);
        // (pointer-to-row-top grab offset, current insertion slot)
        let mut drag: Option<(f32, usize)> = ctx.data_mut(|d| d.get_temp(drag_id));
        let mut disp: HashMap<PathBuf, f32> =
            ctx.data_mut(|d| d.get_temp(disp_id)).unwrap_or_default();
        let mut pal: HashMap<PathBuf, Option<Vec<Color32>>> =
            ctx.data_mut(|d| d.get_temp(pal_id)).unwrap_or_default();
        let mut pal_key: Option<PathBuf> = ctx.data_mut(|d| d.get_temp(palkey_id));
        let pal_col: Option<Color32> = ctx.data_mut(|d| d.get_temp(palcol_id));
        let drag_t: f32 = ctx.data_mut(|d| d.get_temp(dragt_id)).unwrap_or(0.0);

        let rows: Vec<(usize, PathBuf, String, String, Option<f64>)> = carousel
            .item_rows()
            .map(|(i, p, t, a, d)| (i, p.to_path_buf(), t.to_string(), a.to_string(), d))
            .collect();
        let n = rows.len();
        let mut is_sel: Vec<bool> = rows.iter().map(|(_, p, ..)| sel.contains(p)).collect();
        let mut m = is_sel.iter().filter(|b| **b).count();
        let sel_bg = pal_col.unwrap_or(accent);

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
        let mut animating = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(&mut child, |ui| {
                let w = ui.available_width();
                let content_y0 = ui.min_rect().min.y;
                if let Some((_, t)) = drag.as_mut() {
                    if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                        *t = ((pos.y - content_y0) / row_h)
                            .round()
                            .clamp(0.0, n.saturating_sub(m) as f32)
                            as usize;
                    }
                }

                let draw_row =
                    |lp: &egui::Painter,
                     r: Rect,
                     i: usize,
                     title: &str,
                     album: &str,
                     dur: Option<f64>,
                     fg: Color32| {
                        let cy = r.center().y;
                        lp.text(
                            Pos2::new(r.min.x + idx_w - 0.06 * h, cy),
                            Align2::RIGHT_CENTER,
                            format!("{}.", i + 1),
                            font.clone(),
                            fg.gamma_multiply(0.65),
                        );
                        // Filenames keep their "NN." track number —
                        // the index column already shows it, so drop
                        // the filename's copy (double numeration).
                        let title = strip_track_no(title);
                        let text = if album.is_empty() {
                            title.to_string()
                        } else {
                            format!("{title} - {album}")
                        };
                        let text_x = r.min.x + idx_w + 0.066 * h;
                        let text = ellipsize(
                            lp,
                            &text,
                            &font,
                            r.max.x - dur_right - dur_w - col_gap - text_x,
                        );
                        crate::emoji::rich_text(
                            lp,
                            &ctx,
                            Pos2::new(text_x, cy),
                            &text,
                            &font,
                            fg,
                        );
                        lp.text(
                            Pos2::new(r.max.x - dur_right, cy),
                            Align2::RIGHT_CENTER,
                            dur.map(fmt_time).unwrap_or_else(|| "--:--".into()),
                            font.clone(),
                            fg,
                        );
                    };
                let lp = ui.painter().with_clip_rect(list);

                let mut sel_dirty = false;
                for (i, path, title, album, dur) in rows.iter() {
                    let (rect, resp) = ui.allocate_exact_size(
                        Vec2::new(w, row_h),
                        egui::Sense::click_and_drag(),
                    );
                    let selected = sel.contains(path);
                    let playing = playing_track == Some(path.as_path());
                    if interactive && drag.is_none() {
                        if resp.hovered() {
                            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            let mods = ctx.input(|i| i.modifiers);
                            if mods.ctrl || mods.command {
                                if !sel.remove(path) {
                                    sel.insert(path.clone());
                                }
                            } else if mods.shift {
                                let (a, b) = (anchor.min(*i), anchor.max(*i));
                                for (j, p2, ..) in rows.iter() {
                                    if *j >= a && *j <= b {
                                        sel.insert(p2.clone());
                                    }
                                }
                            } else {
                                sel.clear();
                                sel.insert(path.clone());
                            }
                            anchor = *i;
                            sel_dirty = true;
                        }
                        if resp.double_clicked() {
                            out.play = Some(path.clone());
                        }
                        if resp.drag_started() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if !selected {
                                    sel.clear();
                                    sel.insert(path.clone());
                                    anchor = *i;
                                    sel_dirty = true;
                                }
                                drag = Some((pos.y - rect.min.y, *i));
                            }
                        }
                    }
                    if sel_dirty {
                        is_sel = rows.iter().map(|(_, p, ..)| sel.contains(p)).collect();
                        m = is_sel.iter().filter(|b| **b).count();
                        sel_dirty = false;
                    }
                    // While dragging, selected rows leave their slots
                    // and ride the pointer stack — the gap is where
                    // the dashed holes are.
                    if drag.is_some() && is_sel[*i] {
                        continue;
                    }
                    let slot = if let Some((_, t)) = drag {
                        let p = *i - is_sel[..*i].iter().filter(|b| **b).count();
                        (p + if p >= t { m } else { 0 }) as f32
                    } else {
                        *i as f32
                    };
                    let cur0 = disp.get(path).copied().unwrap_or(slot);
                    let cur = if (cur0 - slot).abs() < 0.02 {
                        slot
                    } else {
                        animating = true;
                        cur0 + (slot - cur0) * follow
                    };
                    disp.insert(path.clone(), cur);
                    let r = rect.translate(Vec2::new(0.0, (cur - *i as f32) * row_h));
                    if !r.intersects(list) {
                        continue;
                    }
                    if selected {
                        lp.rect_filled(r, 0.08 * h, sel_bg.gamma_multiply(0.45));
                        lp.rect_stroke(
                            r,
                            0.08 * h,
                            egui::Stroke::new(1.0, sel_bg.gamma_multiply(0.85)),
                            egui::StrokeKind::Inside,
                        );
                    } else if playing {
                        lp.rect_filled(r, 0.08 * h, accent.gamma_multiply(0.35));
                    } else if resp.hovered() && interactive {
                        lp.rect_filled(r, 0.08 * h, Color32::from_white_alpha(10));
                    }
                    let fg = if selected || playing {
                        Color32::WHITE
                    } else {
                        idle_fg.gamma_multiply(0.7)
                    };
                    draw_row(&lp, r, *i, title, album, *dur, fg);
                }

                // Dashed drop-slot holes + the floating stack.
                if let Some((grab, t)) = drag {
                    for j in 0..m {
                        let hole = Rect::from_min_size(
                            Pos2::new(list.min.x, content_y0 + (t + j) as f32 * row_h),
                            Vec2::new(w, row_h),
                        );
                        album_row::dashed_rect(
                            &lp,
                            hole.shrink(1.5),
                            0.09 * h,
                            0.02 * h,
                            Color32::from_white_alpha(60),
                        );
                    }
                    if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                        let top = pos.y - grab;
                        let mut j = 0;
                        for (i, path, title, album, dur) in rows.iter() {
                            if !is_sel[*i] {
                                continue;
                            }
                            // A timed blend sweeps the row from its
                            // slot into the pointer stack over ~0.5s
                            // (ease_in_out on the blend factor, so it
                            // lands softly even as the target moves).
                            // Past that the row is pinned — stack drags
                            // track the pointer with no lag.
                            let tgt = (top + j as f32 * row_h - content_y0) / row_h;
                            let cur0 = disp.get(path).copied().unwrap_or(*i as f32);
                            let e = ease_in_out_quad((drag_t / 0.5).min(1.0));
                            let cur = if e >= 1.0 {
                                tgt
                            } else {
                                animating = true;
                                cur0 + (tgt - cur0) * e
                            };
                            disp.insert(path.clone(), cur);
                            let fr = Rect::from_min_size(
                                Pos2::new(list.min.x, content_y0 + cur * row_h),
                                Vec2::new(w, row_h),
                            );
                            lp.rect_filled(
                                fr,
                                0.08 * h,
                                Color32::from_black_alpha(200),
                            );
                            lp.rect_stroke(
                                fr,
                                0.08 * h,
                                egui::Stroke::new(1.0, sel_bg.gamma_multiply(0.9)),
                                egui::StrokeKind::Inside,
                            );
                            draw_row(&lp, fr, *i, title, album, *dur, Color32::WHITE);
                            j += 1;
                        }
                        // Edge auto-scroll while dragging.
                        if pos.y < list.min.y + row_h {
                            ui.scroll_to_rect(
                                Rect::from_min_size(
                                    Pos2::new(list.min.x, content_y0 - row_h),
                                    Vec2::new(1.0, row_h),
                                ),
                                Some(egui::Align::Min),
                            );
                            ctx.request_repaint();
                        } else if pos.y > list.max.y - row_h {
                            ui.scroll_to_rect(
                                Rect::from_min_size(
                                    Pos2::new(list.min.x, content_y0 + n as f32 * row_h),
                                    Vec2::new(1.0, row_h),
                                ),
                                Some(egui::Align::Max),
                            );
                            ctx.request_repaint();
                        }
                    }
                }
                if n == 0 {
                    lp.text(
                        list.center(),
                        Align2::CENTER_CENTER,
                        "Nothing loaded",
                        font.clone(),
                        Color32::from_gray(120),
                    );
                }
            });

        // Commit on release — the moved rows glide home from their
        // floating slots via `disp`. Esc cancels mid-drag instead:
        // nothing commits and the rows glide back the same way (the
        // key is consumed so the global Esc ladder never sees it).
        if let Some((grab, t)) = drag {
            let esc = interactive
                && ctx.input_mut(|i| {
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                });
            if esc {
                let _ = (grab, t);
                drag = None;
                animating = true;
            } else if !ctx.input(|i| i.pointer.primary_down()) {
                let from: Vec<usize> = is_sel
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b)
                    .map(|(i, _)| i)
                    .collect();
                if !from.is_empty() {
                    carousel.move_items(&from, t);
                    animating = true;
                }
                let _ = grab;
                drag = None;
            }
        }
        disp.retain(|k, _| rows.iter().any(|(_, p, ..)| p == k));
        if animating {
            ctx.request_repaint();
        }
        // Selection tint = the anchor item's cover palette, decoded +
        // extracted on a worker and cached per path (`pal` map).
        let sel_key = rows
            .get(anchor)
            .filter(|(_, p, ..)| sel.contains(p))
            .map(|(_, p, ..)| p.clone());
        if pal_key != sel_key {
            pal_key = sel_key.clone();
            match &sel_key {
                Some(kp) => match pal.get(kp) {
                    Some(Some(cols)) => {
                        ctx.data_mut(|d| match cols.first() {
                            Some(c) => {
                                d.insert_temp(palcol_id, *c);
                            }
                            None => {
                                d.remove::<Color32>(palcol_id);
                            }
                        });
                    }
                    Some(None) => {}
                    None => {
                        pal.insert(kp.clone(), None);
                        let src = carousel.cover_for(kp);
                        let kp2 = kp.clone();
                        let ctx2 = ctx.clone();
                        std::thread::spawn(move || {
                            let cols = src
                                .as_ref()
                                .and_then(crate::library::decode_cover)
                                .map(|img| spectrum_bar::dominant_colors(&img, 3));
                            ctx2.data_mut(|d| {
                                let mut mm: HashMap<PathBuf, Option<Vec<Color32>>> =
                                    d.get_temp(pal_id).unwrap_or_default();
                                mm.insert(kp2.clone(), cols.clone());
                                d.insert_temp(pal_id, mm);
                                if d.get_temp::<PathBuf>(palkey_id).as_ref() == Some(&kp2) {
                                    match cols.as_ref().and_then(|c| c.first()) {
                                        Some(c) => {
                                            d.insert_temp(palcol_id, *c);
                                        }
                                        None => {
                                            d.remove::<Color32>(palcol_id);
                                        }
                                    };
                                }
                            });
                            ctx2.request_repaint();
                        });
                    }
                },
                None => ctx.data_mut(|d| d.remove::<Color32>(palcol_id)),
            }
        }
        ctx.data_mut(|d| {
            d.insert_temp(sel_id, sel);
            d.insert_temp(anchor_id, anchor);
            match drag {
                Some(v) => {
                    d.insert_temp(drag_id, v);
                    // The gather blend keeps advancing while dragging.
                    d.insert_temp(dragt_id, drag_t + dt);
                }
                None => {
                    d.remove::<(f32, usize)>(drag_id);
                    d.remove::<f32>(dragt_id);
                }
            }
            d.insert_temp(disp_id, disp);
            d.insert_temp(pal_id, pal);
            match &pal_key {
                Some(v) => {
                    d.insert_temp(palkey_id, v.clone());
                }
                None => {
                    d.remove::<PathBuf>(palkey_id);
                }
            }
        });
    }

    // The position text and the marquee move on their own — keep
    // frames coming while playback runs (event-driven painting would
    // freeze them on the last drawn frame).
    if matches!(
        player.map(|p| p.state()),
        Some(audio::PlayerState::Playing)
    ) {
        ctx.request_repaint();
    }

    out
}

/// Widest duration cell across the loaded items — the playlist's
/// SharedSizeGroup "DurationColumn" equivalent ({"--:--"} when a tag
/// hasn't resolved yet, e.g. streams).
fn rows_dur_w(painter: &egui::Painter, font: &FontId, carousel: &carousel::Carousel) -> f32 {
    carousel
        .item_rows()
        .map(|(.., d)| {
            painter
                .layout_no_wrap(
                    d.map(fmt_time).unwrap_or_else(|| "--:--".into()),
                    font.clone(),
                    Color32::BLACK,
                )
                .rect
                .width()
        })
        .fold(0.0, f32::max)
}

/// Drop a leading "NN." track number from a display title — the
/// list's index column owns numeration.
fn strip_track_no(t: &str) -> &str {
    let t = t.trim_start();
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    match t[digits..].strip_prefix('.') {
        Some(rest) if digits > 0 => rest.trim_start(),
        _ => t,
    }
}

/// QuadraticEaseInOut — the ExtensionArea drawer's 0.62s easing.
fn ease_in_out_quad(t: f32) -> f32 {
    if t < 0.5 {
        2.0 * t * t
    } else {
        -1.0 + (4.0 - 2.0 * t) * t
    }
}

/// Lacrima's MinView speaker glyph (120x100 canvas): box + horn,
/// three right-bulging wave arcs when unmuted, an X when muted.
fn speaker(painter: &egui::Painter, r: Rect, muted: bool, col: Color32) {
    let k = (r.width() / 120.0).min(r.height() / 100.0);
    let ox = r.min.x + (r.width() - 120.0 * k) * 0.5;
    let oy = r.min.y + (r.height() - 100.0 * k) * 0.5;
    let at = |u: f32, v: f32| Pos2::new(ox + u * k, oy + v * k);
    painter.rect_filled(
        Rect::from_min_max(at(10.0, 34.0), at(32.0, 66.0)),
        egui::CornerRadius::same((4.0 * k).round() as u8),
        col,
    );
    painter.add(egui::Shape::convex_polygon(
        vec![at(32.0, 34.0), at(52.0, 18.0), at(52.0, 82.0), at(32.0, 66.0)],
        col,
        egui::Stroke::NONE,
    ));
    if muted {
        let st = egui::Stroke::new(8.0 * k, col);
        painter.line_segment([at(72.0, 38.0), at(92.0, 58.0)], st);
        painter.line_segment([at(92.0, 38.0), at(72.0, 58.0)], st);
    } else {
        for (cx, rad) in [(64.0, 14.0), (76.0, 28.0)] {
            let pts: Vec<Pos2> = (0..=12)
                .map(|i| {
                    let a = (-90.0 + i as f32 * 15.0).to_radians();
                    Pos2::new(
                        ox + (cx + rad * a.cos()) * k,
                        oy + (50.0 + rad * a.sin()) * k,
                    )
                })
                .collect();
            painter.add(egui::Shape::line(pts, egui::Stroke::new(6.0 * k, col)));
        }
    }
}
