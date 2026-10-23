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
    album_row, audio, carousel, ellipsize, equalizer, fmt_time, library, search_pill,
    settings, spectrum_bar, transport, waveform_bar,
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
/// ~55% of the bar height (Lacrima's 50%, user-tuned +10%).
const CTRL_K: f32 = 0.0055;

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
    /// An album picked from the Tab-toggled albums list (its dir key —
    /// the owner resolves it through open_album).
    pub open: Option<String>,
    /// Confirmed delete from the mini prompt — the owner applies it
    /// (the shell's DeletePrompt paints in the normal view stack,
    /// which mini mode returns before).
    pub delete: Option<crate::PendingDelete>,
    /// Shift+arrows reordered the albums list — the owner persists
    /// library.album_order.
    pub albums_dirty: bool,
    /// A pick from the footer's "+" menu — the owner runs the
    /// picker/prompt (rfd dialogs and url_prompt render outside the
    /// mini view).
    pub menu: Option<MiniMenu>,
    /// Committed inline album-title edit — the owner applies the same
    /// rename path as the strip's `Action::Rename`.
    pub rename: Option<(String, String)>,
    /// Playlist row's "Edit Metadata" — the owner opens the tag
    /// editor modal (its compact layout paints over the mini shell).
    pub edit: Option<PathBuf>,
}

/// The mini "+" menu's entries — mapped onto the same code the
/// carousel/strip context menus run in the main view.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MiniMenu {
    /// Playlist: "Add Files" — local files into the loaded album.
    AddFiles,
    /// Playlist: "Add Url" — open the URL prompt.
    AddUrl,
    /// Playlist: "Add Playlist" — open the playlist URL prompt.
    AddPlaylist,
    /// Albums list: "Create Album" — append an empty virtual album.
    CreateAlbum,
    /// Albums list: "Open Folder" — pick a folder as a music root.
    AddFolder,
}

/// Paint + interact for one frame of the mini shell. `content` is the
/// area below the title bar; geometry follows the small window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn view(
    ui: &mut egui::Ui,
    content: Rect,
    interactive: bool,
    carousel: &mut carousel::Carousel,
    albums: &mut Vec<library::Album>,
    row: &mut album_row::AlbumRow,
    player: Option<&audio::AudioPlayer>,
    playing_track: Option<&Path>,
    // The app's decoded now-playing art — the fallback when the
    // playing item isn't in the loaded album (a stream playing on
    // after an album switch has no `cover_for` row to resolve).
    playing_cover: Option<&egui::TextureHandle>,
    // A fresh track's art is staged until playback is audible —
    // while set, the slot keeps `playing_cover` (the outgoing art)
    // instead of resolving the new item's carousel cover.
    cover_staged: bool,
    stream_loading: bool,
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
    // UI tab's "Show knob" — the pill bars' grab-knob affordance.
    progress_knob: bool,
    // UI tab's "Mini buttons" — the media-controls scale on top of
    // the bar-height sizing.
    btn_scale: f32,
    // The footer-right search pill — one widget per filter so each
    // list keeps its own text: `search` filters the playlist's items
    // (the top-right pill's data), `album_search` the albums (the
    // band pill's data). Only the active list's pill paints.
    search: &mut search_pill::SearchPill,
    album_search: &mut search_pill::SearchPill,
) -> MiniOut {
    let painter = ui.painter().clone();
    painter.rect_filled(content, 0, grid_bg);
    let ctx = ui.ctx().clone();
    let mut out = MiniOut::default();

    // Mini-local delete confirm — the shell's DeletePrompt paints in
    // the normal view stack, which mini mode returns before reaching.
    // While a prompt is pending it owns all input.
    let del_id = ui.scope_id().with("mv-del");
    let mut del: Option<crate::PendingDelete> =
        ctx.data_mut(|d| d.get_temp(del_id)).unwrap_or_default();
    let interactive = interactive && del.is_none();

    // Mini content scale: the shell is window-proportional (everything
    // is a fraction of the bar height `h`), so the UI scale slider
    // scales `h` rather than individual metrics. Anchored at
    // ScaleFactor 2.0 — the factor the layout was tuned at — so 1.0
    // zooms everything to 2x and 3.0 densifies to ~2/3, matching the
    // normal shell's direction (higher factor = denser).
    // Capped at half the window height so a big zoom factor can't
    // push the rows below the bar off the bottom of a small window.
    let h = (BAR_H * s * 2.0).min(content.height() * 0.5);

    // The header gradient is ONE continuous wash from the window top
    // (behind the title bar — its fill is transparent in mini mode)
    // through the now-playing bar, fading over ~two bar heights into
    // the background. The scope painter is clipped to `content`, so
    // this uses a clip widened to the title zone.
    let title_h = crate::MINI_TITLE_BAR_HEIGHT * s;
    let header = Rect::from_min_max(
        Pos2::new(content.min.x, content.min.y - title_h),
        Pos2::new(content.max.x, content.min.y + 2.0 * h),
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
    let bar_rect = Rect::from_min_size(content.min, Vec2::new(content.width(), h));
    // Lacrima insets the bar's content horizontally (~12px).
    let pad_x = 0.13 * h;
    let pad_y = 0.06 * h;
    let cy = bar_rect.center().y;

    // Media controls, right-packed at the bar's edge (the full
    // band's center cluster at bar-relative scale). The UI tab's
    // "Mini buttons" slider scales them on top — clamped so the
    // play button (PLAY_BTN x cs) can never exceed the bar height
    // (0.55 x scale <= 1.0 -> scale <= ~1.8) and can't invert.
    let cs = h * CTRL_K * btn_scale.clamp(0.5, 1.6);
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
    let art = staged_cover(
        cover_staged,
        || {
            playing_track
                .and_then(|t| carousel.cover_for(t))
                .and_then(|src| row.cover_tex(&ctx, &src))
        },
        playing_cover.cloned(),
    );
    // The displayed art's crossfade: every texture-id swap re-arms
    // it — a stage promoting (outgoing → new art), a late cover_tex
    // load (glyph → art), a no-art track (art → glyph). It runs on
    // the palette fade's clock + easing so cover and recolor land
    // together.
    let art_last_id = ui.scope_id().with("mv-art-last");
    let art_fade_id = ui.scope_id().with("mv-art-fade");
    let prev_art: Option<egui::TextureHandle> =
        ctx.data_mut(|d| d.get_temp(art_last_id)).unwrap_or_default();
    if art.as_ref().map(|t| t.id()) != prev_art.as_ref().map(|t| t.id()) {
        ctx.data_mut(|d| {
            d.insert_temp(
                art_fade_id,
                Some((prev_art.clone(), std::time::Instant::now())),
            );
            d.insert_temp(art_last_id, art.clone());
        });
    }
    let art_fade: ArtFade =
        ctx.data_mut(|d| d.get_temp(art_fade_id)).unwrap_or_default();
    let fade_a = art_fade.as_ref().map_or(1.0, |(_, st)| {
        crate::ease((st.elapsed().as_secs_f32() / crate::PALETTE_FADE_S).min(1.0))
    });
    if let Some((_, st)) = &art_fade {
        if st.elapsed().as_secs_f32() >= crate::PALETTE_FADE_S {
            ctx.data_mut(|d| {
                d.insert_temp::<ArtFade>(art_fade_id, None);
            });
        } else {
            ctx.request_repaint();
        }
    }
    // Uniform fit — preserve the art's aspect, centered in the square
    // slot (a 16:9 thumbnail letterboxes like Lacrima, never stretches).
    let fit_rect = |tex: &egui::TextureHandle| {
        let [tw, th] = tex.size();
        let aspect = tw as f32 / (th as f32).max(1.0);
        let fit = if aspect >= 1.0 {
            Vec2::new(cover, cover / aspect)
        } else {
            Vec2::new(cover * aspect, cover)
        };
        Rect::from_center_size(cover_rect.center(), fit)
    };
    // The outgoing layer fades out underneath — the previous art, or
    // the note glyph when the slot was empty.
    if let Some((from, _)) = &art_fade {
        match from {
            Some(tex) => {
                painter.image(
                    tex.id(),
                    fit_rect(tex),
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::from_white_alpha(((1.0 - fade_a) * 255.0).round() as u8),
                );
            }
            None => note_glyph(&painter, cover_rect, cover, 1.0 - fade_a),
        }
    }
    match &art {
        Some(tex) => {
            painter.image(
                tex.id(),
                fit_rect(tex),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::from_white_alpha((fade_a * 255.0).round() as u8),
            );
        }
        None => note_glyph(&painter, cover_rect, cover, fade_a),
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
    // overflows the lane, ellipsized otherwise. A playing item the
    // loaded album doesn't contain (stream playing on across an
    // album switch) misses `display_for` → file-stem fallback shows
    // the raw URL; meta_cache resolves those by their stream key.
    let title = playing_track
        .map(|t| {
            let meta = (carousel.index_of(t).is_none() && library::is_url(t))
                .then(|| crate::meta_cache::load(&t.to_string_lossy()))
                .flatten();
            now_playing_title(carousel.display_for(t).0, meta)
        })
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
            let clip = painter.with_clip_rect(lane);
            crate::emoji::rich_text(
                &clip,
                Pos2::new(lane.min.x + off, lane.center().y),
                &title,
                &font,
                Color32::WHITE,
            );
        } else {
            crate::emoji::rich_text(
                &painter,
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
                    .or_else(|| {
                        // Same cross-album gap as the title — the
                        // stream meta carries its resolved duration.
                        library::is_url(t)
                            .then(|| crate::meta_cache::load(&t.to_string_lossy()))
                            .flatten()
                            .and_then(|m| m.duration)
                    })
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
    out.seek = bar.mini_ui(
        ui,
        track,
        pos_s2,
        dur_s2,
        interactive,
        "mv-seek",
        false,
        progress_knob,
    );

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
            ui.interact(r, ui.scope_id().with(id), egui::Sense::click())
        } else {
            ui.interact(r, ui.scope_id().with(id), egui::Sense::hover())
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
        ui.scope_id().with("mv-vol-pill"),
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
        if let Some(v) = bar.mini_ui(
            ui, slider, vol as f64, 1.0, interactive, "mv-vol", true, progress_knob,
        ) {
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
    let p_id = egui::Id::unique("mv-ext-p");
    let shown_id = egui::Id::unique("mv-ext-shown");
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

    // Footer center: "{playing index} of {count} - {total duration}"
    // — Lacrima's centered #BDBDBD TextBlock (1-based Index of the
    // loaded item, CoverItems count, summed Duration in hh:mm:ss).
    if carousel.len() > 0 {
        let idx = carousel
            .item_rows()
            .find(|(_, p, ..)| Some(*p) == playing_track)
            .map(|(i, ..)| i + 1)
            .unwrap_or(0);
        let total = carousel.total_duration();
        let ts = total.max(0.0) as u64;
        let dur = if ts >= 3600 {
            format!("{:02}:{:02}:{:02}", ts / 3600, (ts % 3600) / 60, ts % 60)
        } else {
            format!("{:02}:{:02}", ts / 60, ts % 60)
        };
        let font = FontId::proportional(0.13 * h); // Lacrima's 12 in the 90px world
        let text = ellipsize(
            &painter,
            &format!("{idx} of {} - {dur}", carousel.len()),
            &font,
            content.width() - 28.0 * u,
        );
        painter.text(
            Pos2::new(content.center().x, (foot_top + content.max.y) * 0.5),
            Align2::CENTER_CENTER,
            text,
            font,
            Color32::from_rgb(0xBD, 0xBD, 0xBD),
        );
    }

    // "+" add-menu — bottom-left of the footer, mirroring the resize
    // dots on the right. Playlist shows Lacrima's adds (files, url,
    // playlist); the albums list shows Create Album / Open Folder.
    let fbtn_cy = (foot_top + content.max.y) * 0.5 - 0.012 * h;
    let pb = Rect::from_center_size(
        Pos2::new(content.min.x + 0.16 * h, fbtn_cy),
        Vec2::splat(0.1615 * h),
    );
    let presp = ui.interact(pb, ui.scope_id().with("mv-plus"), egui::Sense::click());
    painter.rect_stroke(
        pb,
        0.037 * h,
        egui::Stroke::new(
            (0.010 * h).max(1.0),
            if presp.hovered() && interactive {
                Color32::WHITE
            } else {
                Color32::from_white_alpha(140)
            },
        ),
        egui::StrokeKind::Inside,
    );
    let (arm, th) = (0.0366 * h, 0.0133 * h);
    for size in [Vec2::new(arm * 2.0, th), Vec2::new(th, arm * 2.0)] {
        painter.rect_filled(
            Rect::from_center_size(pb.center(), size),
            th * 0.5,
            Color32::WHITE,
        );
    }
    let albums_open: bool = ctx
        .data_mut(|d| d.get_temp(ui.scope_id().with("mv-albums")))
        .unwrap_or(false);
    if interactive {
        let menu_font = FontId::proportional(0.13 * h);
        let items: &[(&str, MiniMenu)] = if albums_open {
            &[
                ("Create Album", MiniMenu::CreateAlbum),
                ("Open Folder", MiniMenu::AddFolder),
            ]
        } else {
            &[
                ("Add Files", MiniMenu::AddFiles),
                ("Add Url", MiniMenu::AddUrl),
                ("Add Playlist", MiniMenu::AddPlaylist),
            ]
        };
        egui::Popup::menu(&presp)
            .align(egui::RectAlign::TOP_START)
            .show(|ui| {
                ui.set_min_width(1.3 * h);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                for (label, act) in items {
                    if ui
                        .button(egui::RichText::new(*label).font(menu_font.clone()))
                        .clicked()
                    {
                        out.menu = Some(*act);
                        ui.close();
                    }
                }
            });
    }

    // Grid toggle beside the "+" — the visible twin of the Tab key:
    // flips between the playlist and the albums list. Bright while
    // the albums list shows, dimmed on the playlist.
    let gb = Rect::from_center_size(
        Pos2::new(pb.max.x + 0.16 * h, pb.center().y),
        Vec2::splat(0.19 * h),
    );
    let gresp = ui.interact(gb, ui.scope_id().with("mv-grid"), egui::Sense::click());
    let (cell, gap) = (0.031 * h, 0.016 * h);
    let span = cell * 3.0 + gap * 2.0;
    let on_col = if albums_open {
        Color32::WHITE
    } else if gresp.hovered() && interactive {
        Color32::from_white_alpha(170)
    } else {
        Color32::from_white_alpha(100)
    };
    for r in 0..3 {
        for c in 0..3 {
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(
                        gb.center().x - span * 0.5 + c as f32 * (cell + gap),
                        gb.center().y - span * 0.5 + r as f32 * (cell + gap),
                    ),
                    Vec2::splat(cell),
                ),
                cell * 0.2,
                on_col,
            );
        }
    }
    if gresp.clicked() && interactive {
        ctx.data_mut(|d| {
            d.insert_temp(ui.scope_id().with("mv-albums"), !albums_open);
        });
    }

    // Footer-right search — the expanding pill docked left of the
    // resize dots (the normal view's top-right pill, h-scaled to the
    // footer). It filters whichever list is active: the albums list
    // shows `album_search`'s box, the playlist `search`'s — the same
    // two filters the normal view keeps.
    let s_pill = (footer_h * 0.62) / 35.0;
    let pill = if albums_open {
        &mut *album_search
    } else {
        &mut *search
    };
    pill.ui(
        ui,
        content.max.x - 30.0 * u,
        foot_top + (footer_h - 35.0 * s_pill) * 0.5,
        if albums_open { "Albums" } else { "Tracks" },
        true,
        false,
        1.0,
        interactive,
        s_pill,
    );
    // Esc collapses an open pill — the shell's Esc ladder lives in
    // the normal view stack mini returns before.
    if interactive && (search.is_open() || album_search.is_open()) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            search.close(&ctx);
            album_search.close(&ctx);
        }
    }
    // Ctrl+F focuses the active list's pill — the normal view's
    // binding.
    if interactive
        && !ctx.text_edit_focused()
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F))
    {
        if albums_open {
            album_search.open();
        } else {
            search.open();
        }
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
    // and the fade live in the same vertices. `scale` folds the
    // slot's crossfade into the ramp so the backdrop swaps in step.
    let art_backdrop = |tex: &egui::TextureHandle, scale: f32| {
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
            let a = (51.0 * scale) as u8;
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
    };
    // The outgoing cover fades out under the incoming one — same
    // alpha the slot uses.
    if let Some((Some(from), _)) = &art_fade {
        art_backdrop(from, 1.0 - fade_a);
    }
    if let Some(tex) = &art {
        art_backdrop(tex, fade_a);
    }
    // The busy pulse/spinner needs frames until the stream opens.
    if stream_loading {
        ctx.request_repaint();
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
        let sel_id = ui.scope_id().with("mv-sel");
        let anchor_id = ui.scope_id().with("mv-sel-anchor");
        let drag_id = ui.scope_id().with("mv-list-drag");
        let disp_id = ui.scope_id().with("mv-list-disp");
        let pal_id = ui.scope_id().with("mv-sel-pal");
        let palkey_id = ui.scope_id().with("mv-sel-pal-key");
        let palcol_id = ui.scope_id().with("mv-sel-pal-col");
        // Seconds into the current drag — drives the stack gather.
        let dragt_id = ui.scope_id().with("mv-drag-t");
        // The track the list last scrolled to — used to scroll the
        // playing item into view once per track change (the restored
        // session track included).
        let reveal_id = ui.scope_id().with("mv-reveal");

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
        let mut revealed: Option<PathBuf> =
            ctx.data_mut(|d| d.get_temp(reveal_id)).unwrap_or(None);

        // The footer-right search pill filters whichever list is
        // active — `pq` maps onto the cover view's CoverItems filter,
        // `aq` onto the strip's album filter. Reorders stay off while
        // a filter hides rows (the cover view's reorderable rule).
        let pq = search.text.trim().to_lowercase();
        let pvis: HashSet<usize> = if pq.is_empty() {
            HashSet::new()
        } else {
            carousel.visible(&search.text).into_iter().collect()
        };
        if !pq.is_empty() {
            drag = None;
        }

        let rows: Vec<(usize, PathBuf, String, String, Option<f64>)> = carousel
            .item_rows()
            .filter(|(i, ..)| pq.is_empty() || pvis.contains(i))
            .map(|(i, p, t, a, d)| (i, p.to_path_buf(), t.to_string(), a.to_string(), d))
            .collect();
        let n = rows.len();
        let mut is_sel: Vec<bool> = rows.iter().map(|(_, p, ..)| sel.contains(p)).collect();
        let mut m = is_sel.iter().filter(|b| **b).count();
        let sel_bg = pal_col.unwrap_or(accent);

        // Tab swaps the playlist for the albums list — `albums_on`
        // persists, `at` eases the crossfade (0 = playlist,
        // 1 = albums).
        let ab_id = ui.scope_id().with("mv-albums");
        let mut albums_on: bool =
            ctx.data_mut(|d| d.get_temp(ab_id)).unwrap_or(false);
        let asel_id = ui.scope_id().with("mv-asel");
        let mut asel: usize =
            ctx.data_mut(|d| d.get_temp(asel_id)).unwrap_or(0);
        // Dir-keyed selection set + `asel` as anchor — the playlist's
        // sel/anchor pair, keyed by dir so reorder/deletes can't
        // desync it.
        let aselset_id = ui.scope_id().with("mv-asel-set");
        let mut asel_set: HashSet<String> =
            ctx.data_mut(|d| d.get_temp(aselset_id)).unwrap_or_default();
        // Eased + a touch longer than the raw bool animation — the
        // eased value drives both opacities and the slide offsets.
        let at = egui::emath::easing::quadratic_in_out(
            ctx.animate_bool_with_time(ui.scope_id().with("mv-albums-t"), albums_on, 0.3),
        );

        // Reseed when the set can't map to a live album (fresh state,
        // deletions) — the anchor is the implicit selection.
        {
            let an = albums.len();
            if an > 0 {
                asel = asel.min(an - 1);
                asel_set.retain(|d| {
                    albums.iter().any(|a| a.dir.to_string_lossy() == d.as_str())
                });
                if asel_set.is_empty() {
                    asel_set
                        .insert(albums[asel].dir.to_string_lossy().into_owned());
                }
            } else {
                asel = 0;
                asel_set.clear();
            }
        }

        // The active list's filter — `avis` carries REAL album
        // indices so `asel`, the dir-keyed set and the reorder math
        // all stay in album space; only the render loop and arrow nav
        // walk it.
        let aq = album_search.text.trim().to_lowercase();
        let avis: Vec<usize> = if aq.is_empty() {
            (0..albums.len()).collect()
        } else {
            albums
                .iter()
                .enumerate()
                .filter(|(_, a)| a.title.to_lowercase().contains(&aq))
                .map(|(i, _)| i)
                .collect()
        };

        // Keyboard: plain arrows move the one-row selection like a
        // ListBox (collapse to the landing row, scrolled into view);
        // Shift+arrows move the selection block one row — album_row's
        // move_selection semantics on the same items (disp glides the
        // rows to their new slots). In albums mode the same arrows
        // walk the album rows and Enter opens one.
        let mut kreveal: Option<PathBuf> = None;
        let mut a_kreveal = false;
        if interactive && !ctx.text_edit_focused() {
            let (mut mv, mut nav) = (0i32, 0i32);
            let (mut tab, mut enter, mut delreq) = (false, false, false);
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
                    tab = true;
                }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                    enter = true;
                }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::Delete) {
                    delreq = true;
                }
                if i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp) {
                    mv = -1;
                }
                if i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown) {
                    mv = 1;
                }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                    nav = -1;
                }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                    nav = 1;
                }
            });
            if tab {
                albums_on = !albums_on;
                ctx.data_mut(|d| d.insert_temp(ab_id, albums_on));
            }
            if albums_on {
                // Albums list owns the keys — the playlist's rules:
                // plain arrows collapse the selection onto the landing
                // row, Shift+arrows move the selection block one row
                // (album_row's move_selection semantics — `to` counts
                // positions in the list without the moved rows).
                let an = albums.len();
                // Reorders stay off while the filter hides cards — a
                // shown-space move can't map back to album space.
                if mv != 0 && an >= 2 && aq.is_empty() {
                    let from = album_sel_indices(albums, &asel_set, asel);
                    let m = from.len();
                    if m > 0 {
                        let to = (from[0] as i32 + mv)
                            .clamp(0, an as i32 - m as i32)
                            as usize;
                        if !from.iter().enumerate().all(|(j, &i)| i == to + j) {
                            let pr =
                                from.iter().position(|&i| i == asel).unwrap_or(0);
                            move_albums_block(albums, &from, to);
                            asel = to + pr;
                            ctx.data_mut(|d| d.insert_temp(asel_id, asel));
                            out.albums_dirty = true;
                            a_kreveal = true;
                        }
                    }
                } else if nav != 0 && an > 0 {
                    // Arrow nav walks VISIBLE cards — `asel` stays a
                    // real index; a filtered anchor lands on the next
                    // card that survived the query.
                    asel = if aq.is_empty() {
                        (asel as i32 + nav).clamp(0, an as i32 - 1) as usize
                    } else {
                        let pos = avis.iter().position(|&i| i == asel).unwrap_or(0);
                        avis.get(
                            (pos as i32 + nav)
                                .clamp(0, avis.len().saturating_sub(1) as i32)
                                as usize,
                        )
                        .copied()
                        .unwrap_or(asel)
                    };
                    asel_set.clear();
                    asel_set
                        .insert(albums[asel].dir.to_string_lossy().into_owned());
                    ctx.data_mut(|d| d.insert_temp(asel_id, asel));
                    a_kreveal = true;
                }
                if enter {
                    if let Some(a) = albums.get(asel.min(an.saturating_sub(1))) {
                        out.open = Some(a.dir.to_string_lossy().into_owned());
                        // Back to the playlist once an album loads.
                        albums_on = false;
                        ctx.data_mut(|d| d.insert_temp(ab_id, false));
                    }
                }
                if delreq {
                    let dirs: Vec<String> = albums
                        .iter()
                        .filter(|a| {
                            asel_set.contains(a.dir.to_string_lossy().as_ref())
                        })
                        .map(|a| a.dir.to_string_lossy().into_owned())
                        .collect();
                    if !dirs.is_empty() {
                        del = Some(crate::PendingDelete::Albums(dirs));
                        ctx.data_mut(|d| {
                            d.insert_temp(ui.scope_id().with("mv-del-sel"), 1usize);
                        });
                    }
                }
            } else if delreq {
                let paths: Vec<PathBuf> = sel.iter().cloned().collect();
                if !paths.is_empty() {
                    del = Some(crate::PendingDelete::Items(paths));
                    ctx.data_mut(|d| {
                        d.insert_temp(ui.scope_id().with("mv-del-sel"), 1usize);
                    });
                }
            } else if mv != 0 && m > 0 && n >= 2 && pq.is_empty() {
                let from: Vec<usize> = is_sel
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b)
                    .map(|(i, _)| i)
                    .collect();
                // `to` counts non-moved positions = the block's final
                // start index (album_row's `bs` clamped to [0, n-m]).
                let to = ((from[0] as i32 + mv).clamp(0, (n - m) as i32)) as usize;
                if to != from[0] {
                    kreveal = rows.get(from[0]).map(|(_, p, ..)| p.clone());
                    carousel.move_items(&from, to);
                    anchor = to;
                }
            } else {
                if nav != 0 && n > 0 {
                    let target = if sel.is_empty() {
                        if nav < 0 { n - 1 } else { 0 }
                    } else {
                        (anchor as i32 + nav).clamp(0, n as i32 - 1) as usize
                    };
                    if let Some((_, p, ..)) = rows.get(target) {
                        sel.clear();
                        sel.insert(p.clone());
                        anchor = target;
                        is_sel = rows.iter().map(|(_, p, ..)| sel.contains(p)).collect();
                        m = 1;
                        kreveal = Some(p.clone());
                    }
                }
                // Enter plays the selection (anchor row wins when it's
                // selected, else the first selected row) — the
                // double-click's keyboard twin.
                if enter {
                    let idx = if is_sel.get(anchor).copied().unwrap_or(false) {
                        anchor
                    } else {
                        is_sel.iter().position(|b| *b).unwrap_or(anchor)
                    };
                    if let Some((_, p, ..)) = rows.get(idx.min(n.saturating_sub(1))) {
                        out.play = Some(p.clone());
                    }
                }
            }
        }

        // Right-click on empty playlist space — the footer "+" menu's
        // playlist items. Registered before the rows so a row's own
        // menu (same items) wins on top.
        if interactive && at < 0.5 {
            let bg = ui.interact(
                list,
                ui.scope_id().with("mv-playlist-bg"),
                egui::Sense::click(),
            );
            bg.context_menu(|ui| {
                ui.set_min_width(1.3 * h);
                ui.spacing_mut().item_spacing.y = 0.0;
                let font = FontId::proportional(0.13 * h);
                for (label, menu) in [
                    ("Add Files", MiniMenu::AddFiles),
                    ("Add Url", MiniMenu::AddUrl),
                    ("Add Playlist", MiniMenu::AddPlaylist),
                ] {
                    if ui
                        .button(egui::RichText::new(label).font(font.clone()))
                        .clicked()
                    {
                        out.menu = Some(menu);
                        ui.close();
                    }
                }
            });
        }

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
        // The albums list owns the same rect while `albums_on` — the
        // playlist keeps painting for the fade but must not claim
        // input: its ScrollArea runs first and would eat the wheel
        // delta, scrollbar drags, and row clicks meant for albums.
        // Keep `.show` running regardless — it must still consume its
        // own scroll_to_me targets or they'd leak into the albums
        // scroll area.
        if albums_on {
            child.disable();
        }
        let mut animating = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            // Own scroll state — the albums list below builds the
            // same UiBuilder on the same rect, so without salts both
            // areas share one egui scroll id and the playlist's
            // clamp would pin the albums offset (a 1-item playlist
            // pins it to 0).
            .id_salt("mv-playlist-scroll")
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
                     fg: Color32,
                     busy: bool| {
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
                            Pos2::new(text_x, cy),
                            &text,
                            &font,
                            fg,
                        );
                        if busy {
                            // Stream resolving — a spinner arc sits in
                            // the duration slot (its "--:--" would
                            // read as a failed lookup).
                            let c = Pos2::new(r.max.x - dur_right - dur_w * 0.5, cy);
                            let rad = dur_w.min(row_h) * 0.28;
                            let a0 = ctx.input(|i| i.time) as f32 * std::f32::consts::TAU;
                            let pts: Vec<Pos2> = (0..=8)
                                .map(|k| {
                                    let a = a0 + k as f32 / 8.0 * std::f32::consts::TAU * 0.72;
                                    c + Vec2::angled(a) * rad
                                })
                                .collect();
                            lp.add(Shape::line(
                                pts,
                                egui::Stroke::new((0.02 * h).max(1.2), fg),
                            ));
                        } else {
                            lp.text(
                                Pos2::new(r.max.x - dur_right, cy),
                                Align2::RIGHT_CENTER,
                                dur.map(fmt_time).unwrap_or_else(|| "--:--".into()),
                                font.clone(),
                                fg,
                            );
                        }
                    };
                let mut lp = ui.painter().with_clip_rect(list);
                lp.set_opacity(1.0 - at);

                let mut sel_dirty = false;
                for (ri, (i, path, title, album, dur)) in rows.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(
                        Vec2::new(w, row_h),
                        egui::Sense::click_and_drag(),
                    );
                    let selected = sel.contains(path);
                    let playing = playing_track == Some(path.as_path());
                    // Keyboard reveal — a nav/Shift-move target scrolls
                    // into view (path-keyed so it survives a reorder
                    // between frames). Minimal scroll: Align::Center
                    // re-snapped every step and read as "jumpy".
                    if kreveal.as_deref() == Some(path.as_path()) {
                        resp.scroll_to_me(None);
                        kreveal = None;
                    }
                    // Follow the loaded track like the cover view's
                    // reveal does — center its row AND own the
                    // selection when it changes, so a session-restored
                    // item arrives selected (palette tint) instead of
                    // needing a click. A live drag keeps its multi-
                    // selection untouched.
                    if playing && revealed.as_deref() != Some(path.as_path()) {
                        resp.scroll_to_me(Some(egui::Align::Center));
                        revealed = Some(path.clone());
                        if drag.is_none() {
                            sel.clear();
                            sel.insert(path.clone());
                            anchor = ri;
                            sel_dirty = true;
                        }
                    }
                    if interactive && drag.is_none() && at < 0.5 {
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
                                // Range over VISIBLE rows — a hidden
                                // row can't join a shift-selection.
                                let (a, b) = (anchor.min(ri), anchor.max(ri));
                                for (j2, (_, p2, ..)) in rows.iter().enumerate() {
                                    if (a..=b).contains(&j2) {
                                        sel.insert(p2.clone());
                                    }
                                }
                            } else {
                                sel.clear();
                                sel.insert(path.clone());
                            }
                            anchor = ri;
                            sel_dirty = true;
                        }
                        if resp.double_clicked() {
                            out.play = Some(path.clone());
                        }
                        // Reorder drags stay off while the filter
                        // hides rows — a shown-space drop can't map
                        // back to item space.
                        if pq.is_empty() && resp.drag_started() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if !selected {
                                    sel.clear();
                                    sel.insert(path.clone());
                                    anchor = ri;
                                    sel_dirty = true;
                                }
                                drag = Some((pos.y - rect.min.y, ri));
                            }
                        }
                        // Right-click a row — the footer "+" menu's
                        // playlist items, then the pointed row's own
                        // delete under the break.
                        resp.context_menu(|ui| {
                            ui.set_min_width(1.3 * h);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let font = FontId::proportional(0.13 * h);
                            for (label, menu) in [
                                ("Add Files", MiniMenu::AddFiles),
                                ("Add Url", MiniMenu::AddUrl),
                                ("Add Playlist", MiniMenu::AddPlaylist),
                            ] {
                                if ui
                                    .button(
                                        egui::RichText::new(label)
                                            .font(font.clone()),
                                    )
                                    .clicked()
                                {
                                    out.menu = Some(menu);
                                    ui.close();
                                }
                            }
                            ui.separator();
                            // The pointed row's own actions — the
                            // tag editor modal like the cover view's
                            // OpenMetadataCommand.
                            if ui
                                .button(
                                    egui::RichText::new("Edit Metadata")
                                        .font(font.clone()),
                                )
                                .clicked()
                            {
                                out.edit = Some(path.clone());
                                ui.close();
                            }
                            if ui
                                .button(
                                    egui::RichText::new("Delete Item")
                                        .font(font),
                                )
                                .clicked()
                            {
                                // Pointing inside a multi-selection
                                // deletes the set (the Delete key's
                                // rule); otherwise just this row.
                                let paths: Vec<PathBuf> =
                                    if selected && sel.len() > 1 {
                                        sel.iter().cloned().collect()
                                    } else {
                                        vec![path.clone()]
                                    };
                                del = Some(crate::PendingDelete::Items(paths));
                                ui.close();
                            }
                        });
                    }
                    if sel_dirty {
                        is_sel = rows.iter().map(|(_, p, ..)| sel.contains(p)).collect();
                        m = is_sel.iter().filter(|b| **b).count();
                        sel_dirty = false;
                    }
                    // While dragging, selected rows leave their slots
                    // and ride the pointer stack — the gap is where
                    // the dashed holes are.
                    if drag.is_some() && is_sel[ri] {
                        continue;
                    }
                    let slot = if let Some((_, t)) = drag {
                        let p = ri - is_sel[..ri].iter().filter(|b| **b).count();
                        (p + if p >= t { m } else { 0 }) as f32
                    } else {
                        ri as f32
                    };
                    let cur0 = disp.get(path).copied().unwrap_or(slot);
                    let cur = if (cur0 - slot).abs() < 0.02 {
                        slot
                    } else {
                        animating = true;
                        cur0 + (slot - cur0) * follow
                    };
                    disp.insert(path.clone(), cur);
                    let r = rect.translate(Vec2::new(0.0, (cur - ri as f32) * row_h));
                    if !r.intersects(list) {
                        continue;
                    }
                    // Resolving stream — the row fill breathes while
                    // the direct url is fetched (a few seconds for
                    // an uncached item).
                    let busy = playing && stream_loading;
                    let pulse = if busy {
                        0.6 + 0.4 * (ctx.input(|i| i.time) as f32 * 4.0).sin().abs()
                    } else {
                        1.0
                    };
                    if selected {
                        lp.rect_filled(r, 0.08 * h, sel_bg.gamma_multiply(0.45 * pulse));
                        lp.rect_stroke(
                            r,
                            0.08 * h,
                            egui::Stroke::new(1.0, sel_bg.gamma_multiply(0.85 * pulse)),
                            egui::StrokeKind::Inside,
                        );
                    } else if playing {
                        lp.rect_filled(r, 0.08 * h, accent.gamma_multiply(0.35 * pulse));
                    } else if resp.hovered() && interactive && at < 0.5 {
                        lp.rect_filled(r, 0.08 * h, Color32::from_white_alpha(10));
                    }
                    let fg = if selected || playing {
                        Color32::WHITE
                    } else {
                        idle_fg.gamma_multiply(0.7)
                    };
                    draw_row(&lp, r, *i, title, album, *dur, fg, busy);
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
                        for (ri, (i, path, title, album, dur)) in rows.iter().enumerate() {
                            if !is_sel[ri] {
                                continue;
                            }
                            // A timed blend sweeps the row from its
                            // slot into the pointer stack over ~0.5s
                            // (ease_in_out on the blend factor, so it
                            // lands softly even as the target moves).
                            // Past that the row is pinned — stack drags
                            // track the pointer with no lag.
                            let tgt = (top + j as f32 * row_h - content_y0) / row_h;
                            let cur0 = disp.get(path).copied().unwrap_or(ri as f32);
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
                            draw_row(&lp, fr, *i, title, album, *dur, Color32::WHITE, false);
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

        // --- Albums list (Tab toggle) — card rows: square cover,
        // bold title, muted artist below when a tag supplies one, and
        // a track count at the right edge.
        //
        // Selection follows the playlist's rules: the selected card
        // tints with its cover's dominant color (`apal`, decoded on a
        // worker per dir) and pointer drags reorder through a floating
        // card + dashed slot (`adrag`, `adisp` glide map, `adrag_t`
        // gather blend).
        let apal_id = ui.scope_id().with("mv-album-pal");
        let mut apal: HashMap<String, Option<Vec<Color32>>> =
            ctx.data_mut(|d| d.get_temp(apal_id)).unwrap_or_default();
        let adrag_id = ui.scope_id().with("mv-album-drag");
        let mut adrag: Option<(f32, usize)> =
            ctx.data_mut(|d| d.get_temp(adrag_id));
        let adisp_id = ui.scope_id().with("mv-album-disp");
        let mut adisp: HashMap<String, f32> =
            ctx.data_mut(|d| d.get_temp(adisp_id)).unwrap_or_default();
        let adragt_id = ui.scope_id().with("mv-album-drag-t");
        let adrag_t: f32 =
            ctx.data_mut(|d| d.get_temp(adragt_id)).unwrap_or(0.0);
        // Press-edge selection bookkeeping — `clicked` fires on
        // release and loses to micro-drags; press-select keeps
        // selection immediate and this flag swallows the release
        // click that would re-run it.
        let press_sel_id = ui.scope_id().with("mv-album-press-sel");
        let mut press_sel: bool =
            ctx.data_mut(|d| d.get_temp(press_sel_id)).unwrap_or(false);
        // The delete dialog's focused-button slot lives on the view
        // ui — grab its id here where `ui` still is that ui.
        let delsel_id = ui.scope_id().with("mv-del-sel");
        // A live album drag can't survive the filter hiding cards —
        // the drop slot would be computed in shown space.
        if !aq.is_empty() {
            adrag = None;
        }
        if at > 0.0 {
            // Artists resolve lazily — one worker pass fills the
            // whole map (dir → artist) so the list never blocks on
            // tag reads.
            let art_id = ui.scope_id().with("mv-artists");
            let artists: HashMap<String, Option<String>> =
                ctx.data_mut(|d| d.get_temp(art_id)).unwrap_or_default();
            let missing: Vec<(String, PathBuf)> = albums
                .iter()
                .filter(|a| {
                    !artists.contains_key(a.dir.to_string_lossy().as_ref())
                })
                .filter_map(|a| {
                    a.tracks
                        .first()
                        .or(a.added.first())
                        .map(|t| (a.dir.to_string_lossy().into_owned(), t.clone()))
                })
                .collect();
            if !missing.is_empty() {
                ctx.data_mut(|d| {
                    let mut mm: HashMap<String, Option<String>> =
                        d.get_temp(art_id).unwrap_or_default();
                    for (dir, _) in &missing {
                        mm.insert(dir.clone(), None);
                    }
                    d.insert_temp(art_id, mm);
                });
                let ctx2 = ctx.clone();
                std::thread::Builder::new()
                    .name("aes-album-artists".into())
                    .spawn(move || {
                        use lofty::file::TaggedFileExt;
                        let mut found = HashMap::new();
                        for (dir, track) in missing {
                            let artist = lofty::read_from_path(&track)
                                .ok()
                                .and_then(|t| {
                                    t.primary_tag()
                                        .or_else(|| t.first_tag())
                                        .and_then(|tag| {
                                            use lofty::tag::Accessor;
                                            tag.artist().map(|s| s.into_owned())
                                        })
                                })
                                .filter(|s: &String| !s.trim().is_empty());
                            found.insert(dir, artist);
                        }
                        ctx2.data_mut(|d| {
                            let mut mm: HashMap<String, Option<String>> =
                                d.get_temp(art_id).unwrap_or_default();
                            mm.extend(found);
                            d.insert_temp(art_id, mm);
                        });
                        ctx2.request_repaint();
                    })
                    .ok();
            }

            // Selected album's cover → palette, decoded off-thread —
            // the playlist's `pal` pipeline keyed by dir.
            let an = albums.len();
            if an > 0 {
                let gi = asel.min(an - 1);
                let dir = albums[gi].dir.to_string_lossy().into_owned();
                if !apal.contains_key(&dir) {
                    apal.insert(dir.clone(), None);
                    if let Some(src) = albums[gi].covers.last().cloned() {
                        let ctx2 = ctx.clone();
                        std::thread::spawn(move || {
                            let cols = crate::library::decode_cover(&src)
                                .map(|img| {
                                    spectrum_bar::dominant_colors(&img, 3)
                                });
                            ctx2.data_mut(|d| {
                                let mut mm: HashMap<
                                    String,
                                    Option<Vec<Color32>>,
                                > = d.get_temp(apal_id).unwrap_or_default();
                                mm.insert(dir, cols);
                                d.insert_temp(apal_id, mm);
                            });
                            ctx2.request_repaint();
                        });
                    }
                }
            }

            // Inline title edit (Create Album / a card's "Rename
            // Album") — (dir, text, armed); `armed` flips once the
            // editor got its first-frame focus+select-all. The strip
            // does the same over the tile's title band in the normal
            // view; here it overlays the card's title text. A global
            // Id — the owner's Create Album handler arms it without
            // knowing the scroll ui's id space.
            let edit_id = egui::Id::unique("mv-album-edit");
            let mut edit: Option<(String, String, bool)> =
                ctx.data_mut(|d| d.get_temp(edit_id));
            // Dir this edit session already scrolled to — the
            // strip's pending_reveal is one-shot; a per-frame
            // scroll_to_me while the editor can't paint would pin
            // the list's scroll.
            let edit_seen_id = egui::Id::unique("mv-album-edit-seen");
            let mut edit_seen: Option<String> =
                ctx.data_mut(|d| d.get_temp(edit_seen_id));

            // The strip's empty-space menu — album-level actions
            // without a card under the pointer. Registered before the
            // cards so their own menus win on top of it.
            if interactive && at > 0.5 {
                let bg = ui.interact(
                    list,
                    ui.scope_id().with("mv-albums-bg"),
                    egui::Sense::click(),
                );
                bg.context_menu(|ui| {
                    ui.set_min_width(1.3 * h);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let font = FontId::proportional(0.13 * h);
                    if ui
                        .button(egui::RichText::new("Create Album").font(font.clone()))
                        .clicked()
                    {
                        out.menu = Some(MiniMenu::CreateAlbum);
                        ui.close();
                    }
                    if ui
                        .button(egui::RichText::new("Open Folder").font(font))
                        .clicked()
                    {
                        out.menu = Some(MiniMenu::AddFolder);
                        ui.close();
                    }
                });
            }

            let mut child2 =
                ui.new_child(egui::UiBuilder::new().max_rect(list));
            // While `at > 0` this keeps painting through the fade-out
            // too — but once `albums_on` flips off, input belongs to
            // the playlist again.
            if !albums_on {
                child2.disable();
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                // See the playlist's salt above — same rect, same
                // builder would collide scroll ids.
                .id_salt("mv-albums-scroll")
                .show(&mut child2, |ui| {
                    let w = ui.available_width();
                    let content_y0 = ui.min_rect().min.y;
                    let an = albums.len();
                    let mut lp = ui.painter().with_clip_rect(list);
                    lp.set_opacity(at);
                    let card_h = 0.5 * h;
                    let pitch = card_h + 0.05 * h;
                    let pad = 0.05 * h;
                    let tfont = FontId::proportional(0.15 * h);
                    let afont = FontId::proportional(0.105 * h);
                    let sfont = FontId::proportional(0.1 * h);
                    // Selected indices — the cards that float during a
                    // drag and the rows that count as "m" below.
                    let afrom = album_sel_indices(albums, &asel_set, asel);
                    let am = afrom.len();
                    // Same insertion-slot tracking as the playlist —
                    // t indexes the list without the selected cards.
                    if let Some((_, t)) = adrag.as_mut() {
                        if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                            *t = ((pos.y - content_y0) / pitch)
                                .round()
                                .clamp(0.0, an.saturating_sub(am) as f32)
                                as usize;
                        }
                    }
                    // One tint for the whole selection — the anchor
                    // album's cover palette (the playlist's pal_col).
                    let acol = albums
                        .get(asel.min(an.saturating_sub(1)))
                        .map(|a| a.dir.to_string_lossy().into_owned())
                        .and_then(|d| {
                            apal.get(&d)
                                .and_then(|o| o.as_ref())
                                .and_then(|c| c.first().copied())
                        })
                        .unwrap_or(sel_bg);
                    let mut paint_card = |lp: &egui::Painter,
                                          card: Rect,
                                          a: &library::Album,
                                          artist: Option<String>,
                                          acol: Color32,
                                          sel_a: bool,
                                          hover: bool| {
                        if sel_a {
                            lp.rect_filled(
                                card,
                                0.09 * h,
                                acol.gamma_multiply(0.45),
                            );
                            lp.rect_stroke(
                                card,
                                0.09 * h,
                                egui::Stroke::new(
                                    1.0,
                                    acol.gamma_multiply(0.85),
                                ),
                                egui::StrokeKind::Inside,
                            );
                        } else if hover {
                            lp.rect_filled(
                                card,
                                0.09 * h,
                                Color32::from_white_alpha(10),
                            );
                        } else {
                            lp.rect_filled(
                                card,
                                0.09 * h,
                                Color32::from_white_alpha(7),
                            );
                        }
                        // Cover — square, uniform-to-fill crop.
                        let cov = card_h - 2.0 * pad;
                        let cr = Rect::from_min_size(
                            Pos2::new(card.min.x + pad, card.center().y - cov * 0.5),
                            Vec2::splat(cov),
                        );
                        lp.rect_filled(cr, 0.07 * h, Color32::from_gray(34));
                        if let Some(tex) =
                            a.covers.last().and_then(|src| row.cover_tex(&ctx, src))
                        {
                            let (tw, th) =
                                (tex.size()[0] as f32, tex.size()[1] as f32);
                            let fill = (cr.width() / tw).max(cr.height() / th);
                            let wr = cr.width() / fill / tw;
                            let hr = cr.height() / fill / th;
                            let u0 = (1.0 - wr) * 0.5;
                            let v0 = (1.0 - hr) * 0.5;
                            let mut mesh = Mesh::with_texture(tex.id());
                            for (px, ux) in [(cr.min.x, u0), (cr.max.x, u0 + wr)] {
                                for (py, vy) in [(cr.min.y, v0), (cr.max.y, v0 + hr)] {
                                    mesh.vertices.push(Vertex {
                                        pos: Pos2::new(px, py),
                                        uv: Pos2::new(ux, vy),
                                        color: Color32::WHITE,
                                    });
                                }
                            }
                            mesh.indices.extend_from_slice(&[0, 2, 3, 0, 3, 1]);
                            lp.with_clip_rect(cr).add(Shape::mesh(mesh));
                            lp.rect_stroke(
                                cr,
                                0.07 * h,
                                egui::Stroke::new(1.0, Color32::from_black_alpha(60)),
                                egui::StrokeKind::Inside,
                            );
                        }
                        // Title + artist (below, muted — image layout).
                        let tx = cr.max.x + 0.12 * h;
                        let cy = card.center().y;
                        let text_r = card.max.x - 0.7 * h;
                        if let Some(art) = artist {
                            lp.text(
                                Pos2::new(tx, cy - 0.11 * h),
                                Align2::LEFT_CENTER,
                                ellipsize(&lp, &a.title, &tfont, text_r - tx),
                                tfont.clone(),
                                Color32::WHITE,
                            );
                            lp.text(
                                Pos2::new(tx, cy + 0.13 * h),
                                Align2::LEFT_CENTER,
                                ellipsize(&lp, &art, &afont, text_r - tx),
                                afont.clone(),
                                Color32::from_gray(160),
                            );
                        } else {
                            lp.text(
                                Pos2::new(tx, cy),
                                Align2::LEFT_CENTER,
                                ellipsize(&lp, &a.title, &tfont, text_r - tx),
                                tfont.clone(),
                                Color32::WHITE,
                            );
                        }
                        // Item count at the card's right edge.
                        lp.text(
                            Pos2::new(card.max.x - 0.10 * h, cy),
                            Align2::RIGHT_CENTER,
                            format!("{} Tracks", a.tracks.len() + a.added.len() + a.url_count),
                            sfont.clone(),
                            Color32::from_gray(150),
                        );
                    };
                    // `avis` carries real album indices — the
                    // selection set, `asel` and the drop math all
                    // stay in album space while only matching cards
                    // paint.
                    for &i in &avis {
                        let a = &albums[i];
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(w, pitch),
                            egui::Sense::click_and_drag(),
                        );
                        let live = interactive && at > 0.5;
                        // The loaded album's own artist comes from its
                        // live items — tag reads haven't landed yet.
                        let dir = a.dir.to_string_lossy().into_owned();
                        let sel_a = asel_set.contains(dir.as_str());
                        let artist = artists
                            .get(dir.as_str())
                            .cloned()
                            .flatten()
                            .or_else(|| {
                                (carousel.album_key() == Some(dir.as_str()))
                                    .then(|| {
                                        carousel
                                            .first_artist()
                                            .map(|s| s.to_string())
                                    })
                                    .flatten()
                            });
                        // The inline title editor's card — scrolled
                        // into view once per arming (the strip's
                        // pending_reveal), then painted below.
                        let editing_this =
                            edit.as_ref().is_some_and(|(d, ..)| *d == dir);
                        if editing_this && edit_seen.as_deref() != Some(dir.as_str()) {
                            resp.scroll_to_me(None);
                            edit_seen = Some(dir.clone());
                        }
                        if live {
                            // Selection lands on PRESS — a release
                            // `clicked` loses to click_and_drag's 6pt
                            // decision, turning a drifting click into
                            // a micro-drag whose ~0.5s gather/drop
                            // reads as the card selecting late.
                            // `press_sel` swallows the release click
                            // so a Ctrl+press add isn't toggled back
                            // off.
                            if !sel_a
                                && resp.is_pointer_button_down_on()
                                && ctx.input(|i| {
                                    i.pointer
                                        .button_pressed(egui::PointerButton::Primary)
                                })
                            {
                                let mods = ctx.input(|i| i.modifiers);
                                if mods.command {
                                    asel_set.insert(dir.clone());
                                } else if mods.shift {
                                    let (a0, b0) = (asel.min(i), asel.max(i));
                                    for j in a0..=b0.min(an - 1) {
                                        asel_set.insert(
                                            albums[j]
                                                .dir
                                                .to_string_lossy()
                                                .into_owned(),
                                        );
                                    }
                                } else {
                                    asel_set.clear();
                                    asel_set.insert(dir.clone());
                                }
                                asel = i;
                                ctx.data_mut(|d| d.insert_temp(asel_id, asel));
                                press_sel = true;
                            }
                            if resp.clicked() {
                                if !press_sel {
                                    // The playlist's click rules: Ctrl
                                    // toggles, Shift extends the range
                                    // from the anchor, plain selects one.
                                    let mods = ctx.input(|i| i.modifiers);
                                    if mods.command {
                                        if !asel_set.remove(dir.as_str()) {
                                            asel_set.insert(dir.clone());
                                        }
                                    } else if mods.shift {
                                        let (a0, b0) = (asel.min(i), asel.max(i));
                                        for j in a0..=b0.min(an - 1) {
                                            asel_set.insert(
                                                albums[j]
                                                    .dir
                                                    .to_string_lossy()
                                                    .into_owned(),
                                            );
                                        }
                                    } else {
                                        asel_set.clear();
                                        asel_set.insert(dir.clone());
                                    }
                                    asel = i;
                                    ctx.data_mut(|d| d.insert_temp(asel_id, asel));
                                }
                            }
                            if resp.double_clicked() {
                                out.open = Some(dir.clone());
                                albums_on = false;
                                ctx.data_mut(|d| d.insert_temp(ab_id, false));
                            }
                            // Like the playlist — no reorder drag
                            // while a filter hides cards.
                            if aq.is_empty() && resp.drag_started() {
                                if let Some(pos) = resp.interact_pointer_pos() {
                                    // Grabbing an unselected card
                                    // selects just it — the playlist's
                                    // rule for pointer drags. Re-check
                                    // the set (press may have selected
                                    // it since `sel_a` was read).
                                    if !asel_set.contains(dir.as_str()) {
                                        asel_set.clear();
                                        asel_set.insert(dir.clone());
                                    }
                                    asel = i;
                                    adrag = Some((pos.y - rect.min.y, i));
                                }
                            }
                            if a_kreveal && i == asel {
                                resp.scroll_to_me(None);
                            }
                            // Card menu — the strip's order: Create up
                            // top, the pointed card's own actions
                            // after the break.
                            resp.context_menu(|ui| {
                                ui.set_min_width(1.3 * h);
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let font = FontId::proportional(0.13 * h);
                                // The pointed card's primary action —
                                // the double-click's menu twin.
                                if ui
                                    .button(
                                        egui::RichText::new("Open Album")
                                            .font(font.clone()),
                                    )
                                    .clicked()
                                {
                                    out.open = Some(dir.clone());
                                    albums_on = false;
                                    ctx.data_mut(|d| {
                                        d.insert_temp(ab_id, false)
                                    });
                                    ui.close();
                                }
                                if ui
                                    .button(
                                        egui::RichText::new("Create Album")
                                            .font(font.clone()),
                                    )
                                    .clicked()
                                {
                                    out.menu = Some(MiniMenu::CreateAlbum);
                                    ui.close();
                                }
                                ui.separator();
                                if ui
                                    .button(
                                        egui::RichText::new("Rename Album")
                                            .font(font.clone()),
                                    )
                                    .clicked()
                                {
                                    edit =
                                        Some((dir.clone(), a.title.clone(), false));
                                    ui.close();
                                }
                                if ui
                                    .button(
                                        egui::RichText::new("Delete Album")
                                            .font(font),
                                    )
                                    .clicked()
                                {
                                    // Pointing inside a multi-
                                    // selection removes the whole set
                                    // (the strip's export/delete
                                    // rule); otherwise just this card.
                                    let dirs: Vec<String> =
                                        if sel_a && asel_set.len() > 1 {
                                            albums
                                                .iter()
                                                .filter(|a| {
                                                    asel_set.contains(
                                                        a.dir
                                                            .to_string_lossy()
                                                            .as_ref(),
                                                    )
                                                })
                                                .map(|a| {
                                                    a.dir.to_string_lossy()
                                                        .into_owned()
                                                })
                                                .collect()
                                        } else {
                                            vec![dir.clone()]
                                        };
                                    del = Some(crate::PendingDelete::Albums(dirs));
                                    ctx.data_mut(|d| {
                                        d.insert_temp(delsel_id, 1usize)
                                    });
                                    ui.close();
                                }
                            });
                        }
                        // Selected cards leave their slots and ride
                        // the pointer — they're painted below.
                        if adrag.is_some() && sel_a {
                            continue;
                        }
                        // Non-selected cards shift to make room at the
                        // drop slot — `adisp` eases them there.
                        let slot = if let Some((_, t)) = adrag {
                            let p =
                                i - afrom.iter().filter(|&&j| j < i).count();
                            (p + if p >= t { am } else { 0 }) as f32
                        } else {
                            i as f32
                        };
                        let cur0 = adisp.get(dir.as_str()).copied().unwrap_or(slot);
                        let cur = if (cur0 - slot).abs() < 0.02 {
                            slot
                        } else {
                            animating = true;
                            cur0 + (slot - cur0) * follow
                        };
                        adisp.insert(dir, cur);
                        // Cards drift up into place while fading in —
                        // the eased `at` makes it read as one motion.
                        let card = rect
                            .shrink2(Vec2::new(0.0, 0.025 * h))
                            .translate(Vec2::new(
                                0.0,
                                (cur - i as f32) * pitch + (1.0 - at) * 0.06 * h,
                            ));
                        if !card.intersects(list) {
                            continue;
                        }
                        let has_artist = artist.is_some();
                        paint_card(
                            &lp,
                            card,
                            a,
                            artist,
                            acol,
                            sel_a,
                            live && resp.hovered(),
                        );
                        // The editor overlays the title text (ui.put —
                        // clicks inside reach it, not the card's hit
                        // test). Enter/click-away commits, Esc cancels.
                        if editing_this && interactive {
                            let cov = card_h - 2.0 * pad;
                            let tx = card.min.x + pad + cov + 0.12 * h;
                            let cy = card.center().y;
                            // Centered on the title's own baseline —
                            // cy - 0.11h with an artist line, cy
                            // without (paint_card's layout).
                            let title_cy = if has_artist {
                                cy - 0.11 * h
                            } else {
                                cy
                            };
                            let edit_rect = Rect::from_min_max(
                                Pos2::new(tx - 0.04 * h, title_cy - 0.12 * h),
                                Pos2::new(card.max.x - 0.55 * h, title_cy + 0.12 * h),
                            );
                            // Outer Some = the editor closed; inner is
                            // the commit (None = Esc or empty title).
                            let closed: Option<Option<(String, String)>> = {
                                let (edir, etext, armed) = edit.as_mut().unwrap();
                                let r = ui.put(
                                    edit_rect,
                                    egui::TextEdit::singleline(etext)
                                        .font(tfont.clone())
                                        .text_color(Color32::WHITE)
                                        .align(egui::Align2::LEFT_CENTER),
                                );
                                if !*armed {
                                    // Focus once with the whole title
                                    // selected — typing replaces it.
                                    let mut st =
                                        egui::text_edit::TextEditState::load(
                                            &ctx, r.id,
                                        )
                                        .unwrap_or_default();
                                    st.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::two(
                                            egui::text::CCursor::new(0),
                                            egui::text::CCursor::new(
                                                etext.chars().count(),
                                            ),
                                        ),
                                    ));
                                    st.store(&ctx, r.id);
                                    r.request_focus();
                                    *armed = true;
                                }
                                if ctx.input(|i| i.key_pressed(egui::Key::Escape))
                                    && r.has_focus()
                                {
                                    Some(None)
                                } else if r.lost_focus() {
                                    Some(Some((
                                        edir.clone(),
                                        etext.trim().to_string(),
                                    )))
                                } else {
                                    None
                                }
                            };
                            match closed {
                                Some(Some((d, t))) => {
                                    if !t.is_empty() {
                                        out.rename = Some((d, t));
                                    }
                                    edit = None;
                                }
                                Some(None) => edit = None,
                                None => {}
                            }
                        }
                    }
                    // Release cleared the press marker — its swallow
                    // job ended with the click (or a drag-away).
                    if !ctx.input(|i| i.pointer.primary_down()) {
                        press_sel = false;
                    }
                    // Dashed drop slots + the floating stack.
                    if let Some((grab, t)) = adrag {
                        let afrom = album_sel_indices(albums, &asel_set, asel);
                        let am = afrom.len();
                        for j in 0..am {
                            let hole = Rect::from_min_size(
                                Pos2::new(
                                    list.min.x,
                                    content_y0 + (t + j) as f32 * pitch,
                                ),
                                Vec2::new(w, card_h),
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
                            let e = ease_in_out_quad((adrag_t / 0.5).min(1.0));
                            let mut j = 0;
                            for &gi in afrom.iter() {
                                let ga = &albums[gi];
                                let dir = ga.dir.to_string_lossy().into_owned();
                                let artist = artists
                                    .get(dir.as_str())
                                    .cloned()
                                    .flatten()
                                    .or_else(|| {
                                        (carousel.album_key() == Some(dir.as_str()))
                                            .then(|| {
                                                carousel
                                                    .first_artist()
                                                    .map(|s| s.to_string())
                                            })
                                            .flatten()
                                    });
                                // A timed blend sweeps each card from
                                // its slot into the pointer stack —
                                // the playlist's same 0.5s gather.
                                let tgt =
                                    (top + j as f32 * pitch - content_y0) / pitch;
                                let cur0 =
                                    adisp.get(dir.as_str()).copied().unwrap_or(gi as f32);
                                let cur = if e >= 1.0 {
                                    tgt
                                } else {
                                    animating = true;
                                    cur0 + (tgt - cur0) * e
                                };
                                adisp.insert(dir, cur);
                                let card = Rect::from_min_size(
                                    Pos2::new(
                                        list.min.x,
                                        content_y0 + cur * pitch + 0.025 * h,
                                    ),
                                    Vec2::new(w, card_h),
                                );
                                paint_card(&lp, card, ga, artist, acol, true, false);
                                j += 1;
                            }
                            // Edge auto-scroll while dragging.
                            if pos.y < list.min.y + pitch {
                                ui.scroll_to_rect(
                                    Rect::from_min_size(
                                        Pos2::new(list.min.x, content_y0 - pitch),
                                        Vec2::new(1.0, pitch),
                                    ),
                                    Some(egui::Align::Min),
                                );
                                ctx.request_repaint();
                            } else if pos.y > list.max.y - pitch {
                                ui.scroll_to_rect(
                                    Rect::from_min_size(
                                        Pos2::new(
                                            list.min.x,
                                            content_y0 + an as f32 * pitch,
                                        ),
                                        Vec2::new(1.0, pitch),
                                    ),
                                    Some(egui::Align::Max),
                                );
                                ctx.request_repaint();
                            }
                        }
                    }
                    if albums.is_empty() {
                        lp.text(
                            list.center(),
                            Align2::CENTER_CENTER,
                            "No albums",
                            font.clone(),
                            Color32::from_gray(120),
                        );
                    }
                });
            // Editor state back to temp — a gone album clears it.
            ctx.data_mut(|d| match &edit {
                Some(e)
                    if albums
                        .iter()
                        .any(|a| a.dir.to_string_lossy() == e.0.as_str()) =>
                {
                    d.insert_temp(edit_id, e.clone());
                    match &edit_seen {
                        Some(v) => {
                            d.insert_temp(edit_seen_id, v.clone());
                        }
                        None => {
                            d.remove::<String>(edit_seen_id);
                        }
                    }
                }
                _ => {
                    d.remove::<(String, String, bool)>(edit_id);
                    d.remove::<String>(edit_seen_id);
                }
            });
        }

        // Albums drag commit — a released pointer lands the card at t
        // (the insertion index in the list without it), Esc cancels
        // and `adisp` glides everything home.
        if let Some((grab, t)) = adrag {
            let esc = interactive
                && ctx.input_mut(|i| {
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                });
            if esc {
                let _ = grab;
                adrag = None;
                animating = true;
            } else if !ctx.input(|i| i.pointer.primary_down()) {
                let from = album_sel_indices(albums, &asel_set, asel);
                let m = from.len();
                let an = albums.len();
                if m > 0 && an >= 2 {
                    let to = t.min(an - m);
                    if !from.iter().enumerate().all(|(j, &i)| i == to + j) {
                        let pr =
                            from.iter().position(|&i| i == asel).unwrap_or(0);
                        move_albums_block(albums, &from, to);
                        asel = to + pr;
                        ctx.data_mut(|d| d.insert_temp(asel_id, asel));
                        out.albums_dirty = true;
                    }
                }
                let _ = grab;
                adrag = None;
            }
        }
        adisp.retain(|k, _| {
            albums.iter().any(|a| a.dir.to_string_lossy() == k.as_str())
        });
        ctx.data_mut(|d| {
            d.insert_temp(apal_id, apal);
            d.insert_temp(adisp_id, adisp);
            d.insert_temp(aselset_id, asel_set);
            d.insert_temp(asel_id, asel);
            d.insert_temp(press_sel_id, press_sel);
            match adrag {
                Some(v) => {
                    d.insert_temp(adrag_id, v);
                    d.insert_temp(adragt_id, adrag_t + dt);
                }
                None => {
                    d.remove::<(f32, usize)>(adrag_id);
                    d.remove::<f32>(adragt_id);
                }
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
            d.insert_temp(reveal_id, revealed);
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

    // Delete confirm — the shell's DeletePrompt paints in the normal
    // view stack mini returns before reaching, so the dialog lives
    // here (same Switch-style widget, same labels).
    {
        let vis_id = ui.scope_id().with("mv-del-vis");
        let sel_id = ui.scope_id().with("mv-del-sel");
        let mut vis: f32 = ctx.data_mut(|d| d.get_temp(vis_id)).unwrap_or(0.0);
        let mut psel: usize = ctx.data_mut(|d| d.get_temp(sel_id)).unwrap_or(1);
        let open = del.is_some();
        if open || vis > 0.001 {
            let ddt = ctx.input(|i| i.stable_dt);
            vis = (vis + ddt / if open { 0.18 } else { -0.14 }).clamp(0.0, 1.0);
            let (title, subtitle) = match &del {
                Some(crate::PendingDelete::Items(p)) => (
                    if p.len() == 1 {
                        "Delete item?".to_string()
                    } else {
                        format!("Delete {} items?", p.len())
                    },
                    "Removed from the album — the files stay on disk.".to_string(),
                ),
                Some(crate::PendingDelete::Albums(d)) => (
                    if d.len() == 1 {
                        "Delete album?".to_string()
                    } else {
                        format!("Delete {} albums?", d.len())
                    },
                    "Removed from the library — the files stay on disk.".to_string(),
                ),
                _ => ("Delete?".to_string(), String::new()),
            };
            // Escape dismisses; the overlay eats Tab/arrows/Enter.
            if open
                && ctx
                    .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                del = None;
            }
            // The dialog is sized in `s` units — scale it down on
            // windows narrower than the fixed panel.
            let ds = s * (content.width() / (440.0 * s)).min(1.0);
            match crate::confirm_overlay(
                ui, content, &title, &subtitle, "Delete", "Cancel", &mut psel, ds,
                vis, open, "mv-del-confirm",
            ) {
                crate::DialogAction::Confirm => out.delete = del.take(),
                crate::DialogAction::Cancel => del = None,
                crate::DialogAction::None => {}
            }
            ctx.data_mut(|d| {
                d.insert_temp(vis_id, vis);
                d.insert_temp(sel_id, psel);
            });
            ctx.request_repaint();
        }
        // Store the whole Option — get_temp downcasts by exact type,
        // so a bare PendingDelete would never read back.
        ctx.data_mut(|d| d.insert_temp(del_id, del.clone()));
    }

    out
}

/// Sorted indices of `set`'s dirs — the albums list's `is_sel`
/// equivalent. Falls back to the anchor when nothing maps (the
/// anchor is the implicit selection).
fn album_sel_indices(
    albums: &[library::Album],
    set: &HashSet<String>,
    anchor: usize,
) -> Vec<usize> {
    let idx: Vec<usize> = albums
        .iter()
        .enumerate()
        .filter(|(_, a)| set.contains(a.dir.to_string_lossy().as_ref()))
        .map(|(i, _)| i)
        .collect();
    if idx.is_empty() && anchor < albums.len() {
        vec![anchor]
    } else {
        idx
    }
}

/// Move the rows at `from` (sorted, non-empty) so the block starts
/// at `to` — `to` indexes the list without the moved rows, matching
/// `carousel::move_items` / `album_row::move_selection` semantics.
/// A non-contiguous `from` collapses into a contiguous run.
fn move_albums_block(albums: &mut Vec<library::Album>, from: &[usize], to: usize) {
    let mut pulled: Vec<library::Album> = Vec::with_capacity(from.len());
    for &i in from.iter().rev() {
        pulled.push(albums.remove(i));
    }
    pulled.reverse();
    let at = to.min(albums.len());
    for (j, a) in pulled.into_iter().enumerate() {
        albums.insert(at + j, a);
    }
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
/// The now-playing bar's title — the carousel's display text, or the
/// stream meta's resolved title when the playing item isn't in the
/// loaded album (play-then-switch left a raw "watch?v=…" stem up top).
fn now_playing_title(
    display: String,
    meta: Option<crate::meta_cache::CachedMeta>,
) -> String {
    meta.filter(|m| !m.title.trim().is_empty())
        .map(|m| m.title)
        .unwrap_or(display)
}

/// Session-temp payload for the mini cover crossfade — the outgoing
/// art (None = the note glyph was showing) and when the swap armed.
type ArtFade = Option<(Option<egui::TextureHandle>, std::time::Instant)>;

/// The mini slot's art while a fresh track's cover is staged for
/// audible playback: the held (displayed) art stands in until the
/// stage promotes; once unstaged the playing item's own art resolves
/// with the displayed cover as fallback.
fn staged_cover<T>(
    staged: bool,
    resolve: impl FnOnce() -> Option<T>,
    held: Option<T>,
) -> Option<T> {
    if staged { held } else { resolve().or(held) }
}

/// Lacrima's mini default cover — a bare note glyph on the palette
/// backdrop, authored on a 400x400 canvas. `alpha` fades it like the
/// art layers during a crossfade.
fn note_glyph(painter: &egui::Painter, slot: Rect, size: f32, alpha: f32) {
    let g = size * 0.55;
    let r = Rect::from_center_size(slot.center(), Vec2::splat(g));
    let k = g / 400.0;
    let at = |u: f32, v: f32| Pos2::new(r.min.x + u * k, r.min.y + v * k);
    // Light glyph — visible on the dark/colored backdrop (Lacrima's
    // is dark only because its tile is light).
    let col = Color32::from_gray(120).gamma_multiply(alpha);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn album(dir: &str) -> library::Album {
        library::Album::virtual_album(dir, dir.to_string(), &[])
    }

    fn dirs(albums: &[library::Album]) -> Vec<String> {
        albums
            .iter()
            .map(|a| a.dir.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn album_sel_indices_maps_dirs_and_falls_back_to_anchor() {
        let albums = vec![album("a"), album("b"), album("c")];
        let set: HashSet<String> = ["a".into(), "c".into()].into_iter().collect();
        assert_eq!(album_sel_indices(&albums, &set, 0), vec![0, 2]);
        // Stale/empty set → the anchor index is the implicit selection.
        let stale: HashSet<String> = ["gone".into()].into_iter().collect();
        assert_eq!(album_sel_indices(&albums, &stale, 1), vec![1]);
        // Anchor out of range → no selection.
        assert!(album_sel_indices(&albums, &stale, 5).is_empty());
    }

    #[test]
    fn move_albums_block_matches_move_items_semantics() {
        // Single card down: [A,B,C,D] move 0→1 → [B,A,C,D].
        let mut a = vec![album("a"), album("b"), album("c"), album("d")];
        move_albums_block(&mut a, &[0], 1);
        assert_eq!(dirs(&a), vec!["b", "a", "c", "d"]);
        // Contiguous block up: [A,B,C,D] move {C,D}→0 → [C,D,A,B].
        let mut a2 = vec![album("a"), album("b"), album("c"), album("d")];
        move_albums_block(&mut a2, &[2, 3], 0);
        assert_eq!(dirs(&a2), vec!["c", "d", "a", "b"]);
        // Non-contiguous selection collapses to a run: {A,D}→1 → [B,A,D,C].
        let mut b = vec![album("a"), album("b"), album("c"), album("d")];
        move_albums_block(&mut b, &[0, 3], 1);
        assert_eq!(dirs(&b), vec!["b", "a", "d", "c"]);
        // `to` indexes the list without the moved rows and clamps.
        let mut c = vec![album("a"), album("b"), album("c")];
        move_albums_block(&mut c, &[0], 3);
        assert_eq!(dirs(&c), vec!["b", "c", "a"]);
    }

    /// A stream still playing after an album switch isn't in the
    /// loaded carousel — its title must come from the resolved meta,
    /// not the URL's file stem (the "watch?v=…" header regression).
    #[test]
    fn now_playing_title_prefers_stream_meta() {
        let meta = crate::meta_cache::CachedMeta {
            title: "Lost in 1989 — Synthwave Mix".into(),
            ..Default::default()
        };
        assert_eq!(
            now_playing_title("watch?v=Qa76lUQkoDw".to_string(), Some(meta)),
            "Lost in 1989 — Synthwave Mix"
        );
        // No meta / blank meta title → the display fallback stands.
        assert_eq!(now_playing_title("fallback".to_string(), None), "fallback");
        let blank = crate::meta_cache::CachedMeta {
            title: "  ".into(),
            ..Default::default()
        };
        assert_eq!(
            now_playing_title("fallback".to_string(), Some(blank)),
            "fallback"
        );
    }

    /// While a fresh track's cover is staged for audible playback the
    /// mini slot must keep showing the outgoing art — resolving the
    /// new item's carousel cover early was the pre-play leak the
    /// stage gates. Once it promotes, the item's own art resolves
    /// with the displayed cover still the cross-album fallback.
    #[test]
    fn staged_cover_holds_displayed_art() {
        // Staged: the held (displayed) cover wins over the resolved.
        assert_eq!(staged_cover(true, || Some(2), Some(1)), Some(1));
        // Staged with nothing displayed → the empty slot, not the art.
        assert_eq!(staged_cover(true, || Some(2), None), None);
        // Unstaged: the playing item's own art resolves normally.
        assert_eq!(staged_cover(false, || Some(2), Some(1)), Some(2));
        // Unstaged, nothing resolvable → the displayed-cover fallback.
        assert_eq!(staged_cover(false, || None, Some(1)), Some(1));
        // Neither → empty.
        assert_eq!(staged_cover(false, || None::<i32>, None), None);
    }
}
