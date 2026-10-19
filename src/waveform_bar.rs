//! The waveform seek bar above the albums strip (Lacrima's
//! CompositionWaveformProgressBar as bound in MusicView.axaml):
//! symmetric bars — DimGray with the played head in RoyalBlue — a 2px
//! gray indicator, a white triangle hanging above the playhead, a
//! hover time tooltip, and the 12-spoke loading spinner. Press/drag
//! scrubs the preview; release returns the seek position.

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2,
};

// Layout constants are the MusicView.axaml bindings / control
// defaults, in base units scaled by `s` at draw time.
const V_OFFSET: f32 = 5.0; // WaveformVerticalOffset
const INDICATOR_W: f32 = 2.0;
const TRI_OFFSET: f32 = -14.0; // TriangleOffset — hangs above the bar
const TRI_W: f32 = 14.0; // TriangleWidth
const TRI_H: f32 = 12.0; // TriangleHeight
const TEXT_SIZE: f32 = 16.0; // TextSize
const TOOLTIP_GAP: f32 = 5.0;
const MIN_BAR_H: f32 = 1.5;
const SPINNER_R: f32 = 15.0; // LoadingIndicatorSize 30 / 2
const SPINNER_SPOKES: usize = 12;

const BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x10); // CardGridBackgroundColor
const PLAYED: Color32 = Color32::from_rgb(0x41, 0x69, 0xE1); // RoyalBlue
const UNPLAYED: Color32 = Color32::from_rgb(0x69, 0x69, 0x69); // DimGray
const INDICATOR: Color32 = Color32::GRAY; // IndicatorColor="Gray"
const TRIANGLE: Color32 = Color32::WHITE;
const TEXT: Color32 = Color32::LIGHT_GRAY; // TextForegroundColor
// MinView's plain progress pill: dark track, light elapsed fill,
// thin lighter outline.
const MINI_TRACK: Color32 = Color32::from_rgb(0x4D, 0x4D, 0x4D);
const MINI_FILL: Color32 = Color32::from_rgb(0xC8, 0xC8, 0xC8);
const MINI_BORDER: Color32 = Color32::from_rgb(0x8A, 0x8A, 0x8A);

/// Scrub state: while the pointer is held the bar previews `scrub`
/// seconds locally (Lacrima's _dragValue); the player only hears
/// about it on release.
#[derive(Default)]
pub struct WaveformBar {
    scrub: Option<f64>,
    /// Released scrub target shown until the player reports it — seek
    /// applies asynchronously, so reverting to `position` instantly
    /// snaps the thumb back to the pre-seek spot for a few frames.
    pending: Option<(f64, std::time::Instant)>,
}

impl WaveformBar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Paint + interact. `position`/`duration` are seconds. Returns the
    /// seek position (seconds) when the user releases a scrub.
    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        waveform: &[f32],
        position: f64,
        duration: f64,
        loading: bool,
        alpha: f32,
        interactive: bool,
        s: f32,
    ) -> Option<f64> {
        let mut seek = None;
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, BG.gamma_multiply(alpha));

        // --- Scrub interaction ----------------------------------------
        let resp = ui.interact(rect, ui.id().with("waveform-bar"), Sense::click_and_drag());
        let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
        if interactive {
            if resp.is_pointer_button_down_on() {
                if let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    let ratio = ((p.x - rect.min.x) / rect.width().max(1.0)) as f64;
                    self.scrub = Some(ratio.clamp(0.0, 1.0) * duration.max(0.0));
                }
            } else if let Some(v) = self.scrub.take() {
                seek = Some(v);
                self.pending = Some((v, std::time::Instant::now()));
            }
        } else if self.scrub.is_some() && !ui.ctx().input(|i| i.pointer.primary_down()) {
            seek = self.scrub.take();
            if let Some(v) = seek {
                self.pending = Some((v, std::time::Instant::now()));
            }
        }

        // Hold the released position until the player converges on it
        // (or the seek silently fails and it times out).
        if let Some((t, when)) = self.pending {
            if (position - t).abs() < 0.25 || when.elapsed().as_secs_f32() > 0.75 {
                self.pending = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        let shown = self.scrub.or(self.pending.map(|(t, _)| t)).unwrap_or(position);
        let progress = if duration > 0.0 {
            (shown / duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        // --- Bars (DrawWaveformSection, symmetric solid mode) ---------
        let h = rect.height();
        let avail_h = (h - V_OFFSET * s).max(0.0);
        let center_y = rect.min.y + (h + V_OFFSET * s) * 0.5;
        let indicator_x = (rect.min.x + progress * rect.width()).round();

        if !waveform.is_empty() && avail_h > 0.0 {
            let count = waveform.len();
            // Gapped capsule bars: rounded pill columns (~35% gap) with
            // a sheen cap, so the strip reads as glossy bars instead of
            // hard-edged pixel columns. Each bar's peak is the max of
            // the samples in its bin.
            let slot = (4.0 * s).max(3.0);
            let bar_w = (slot - 1.4 * s).max(slot * 0.6);
            let n_bars = ((rect.width() + slot - bar_w) / slot).floor() as usize;
            let radius = (bar_w * 0.5).min(2.5 * s);
            let min_h = bar_w.max(MIN_BAR_H * s);
            for pass in 0..2 {
                let base = if pass == 0 { UNPLAYED } else { PLAYED };
                let sheen = if pass == 0 { 0.07 } else { 0.16 };
                let p = match pass {
                    0 => painter.with_clip_rect(rect),
                    _ => painter.with_clip_rect(Rect::from_min_max(
                        rect.min,
                        Pos2::new(indicator_x, rect.max.y),
                    )),
                };
                for b in 0..n_bars {
                    let i0 = b * count / n_bars;
                    let i1 = (((b + 1) * count) / n_bars).max(i0 + 1).min(count);
                    let v = waveform[i0..i1].iter().fold(0.0f32, |a, &v| a.max(v));
                    let bar_h = (v * avail_h).clamp(0.0, avail_h).max(min_h);
                    let cx = rect.min.x + (b as f32 + 0.5) * slot;
                    let r = Rect::from_center_size(
                        Pos2::new(cx, center_y),
                        Vec2::new(bar_w, bar_h),
                    );
                    if r.min.x >= rect.max.x {
                        break;
                    }
                    p.rect_filled(r, radius, base.gamma_multiply(alpha));
                    // Sheen: a lighter cap at the bar's top edge.
                    let cap = Rect::from_min_max(
                        r.min,
                        Pos2::new(r.max.x, r.min.y + (bar_h * 0.22).max(2.0 * s)),
                    );
                    p.rect_filled(
                        cap,
                        egui::CornerRadius {
                            nw: radius as u8,
                            ne: radius as u8,
                            sw: 0,
                            se: 0,
                        },
                        Color32::WHITE.gamma_multiply(sheen * alpha),
                    );
                }
            }
            // Soft glow band around the playhead — two triangles fading
            // from the indicator color to transparent at the edges.
            let glow_w = 10.0 * s;
            let mut gm = egui::epaint::Mesh::default();
            let gc = |a: f32| {
                Color32::from_rgb(0x8B, 0xA7, 0xF9).gamma_multiply(a * alpha)
            };
            for (x, a) in [
                (indicator_x - glow_w, 0.0),
                (indicator_x, 0.16),
                (indicator_x + glow_w, 0.0),
            ] {
                gm.vertices.push(egui::epaint::Vertex {
                    pos: Pos2::new(x, rect.min.y),
                    uv: egui::epaint::WHITE_UV,
                    color: gc(a),
                });
                gm.vertices.push(egui::epaint::Vertex {
                    pos: Pos2::new(x, rect.max.y),
                    uv: egui::epaint::WHITE_UV,
                    color: gc(a),
                });
            }
            gm.indices
                .extend_from_slice(&[0, 1, 3, 0, 3, 2, 2, 3, 5, 2, 5, 4]);
            painter.add(Shape::Mesh(std::sync::Arc::new(gm)));
        }

        // Indicator: full-height 2px line at the playhead.
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(indicator_x - 1.0, rect.min.y),
                Vec2::new(INDICATOR_W, h),
            ),
            0.0,
            INDICATOR.gamma_multiply(alpha),
        );

        // Triangle hanging above the playhead, tip 2px off the bar top
        // (base at TriangleOffset, apex base+TriangleHeight).
        let base_y = rect.min.y + TRI_OFFSET * s;
        painter.add(Shape::convex_polygon(
            vec![
                Pos2::new(indicator_x, base_y + TRI_H * s),
                Pos2::new(indicator_x - TRI_W * 0.5 * s, base_y),
                Pos2::new(indicator_x + TRI_W * 0.5 * s, base_y),
            ],
            TRIANGLE.gamma_multiply(alpha),
            Stroke::NONE,
        ));

        // Hover/scrub time tooltip (hh:mm:ss above the pointer).
        let tip_x = if self.scrub.is_some() {
            ui.ctx().input(|i| i.pointer.interact_pos())
        } else {
            pointer.filter(|p| interactive && rect.contains(*p))
        };
        if let Some(p) = tip_x {
            if duration > 0.0 {
                let ratio = ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0) as f64;
                let t = (ratio * duration).max(0.0) as u64;
                let text = format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60);
                let ty = rect.min.y
                    + (p.y - rect.min.y - (TEXT_SIZE + TOOLTIP_GAP) * s).max(0.0)
                    + TEXT_SIZE * s;
                painter.text(
                    Pos2::new(p.x, ty),
                    Align2::LEFT_BOTTOM,
                    text,
                    FontId::proportional(TEXT_SIZE * s),
                    TEXT.gamma_multiply(alpha),
                );
            }
        }

        // Loading spinner while the waveform decodes.
        if loading {
            self.loading(ui, rect, alpha, s);
        }

        seek
    }

    /// True while the pointer is scrubbing inside the named mini bar
    /// — the mini volume pill stays expanded for the drag's duration.
    /// Each `mini_ui` call site owns its scrub slot keyed by `id`.
    pub fn mini_scrubbing(&self, ctx: &egui::Context, id: &str) -> bool {
        ctx.data_mut(|d| {
            d.get_temp::<f64>(egui::Id::new(("mini-bar-scrub", id)))
                .is_some()
        })
    }

    /// The mini shell's plain seek track — Lacrima's MinView progress
    /// bar: a full-radius pill with the elapsed portion filled light.
    /// Same scrub-hold/pending plumbing as the waveform strip, but
    /// keyed by `id` so coexisting mini bars (seek + volume) can't
    /// steal each other's in-flight drag.
    pub fn mini_ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        position: f64,
        duration: f64,
        interactive: bool,
        id: &str,
        live: bool,
    ) -> Option<f64> {
        let mut seek = None;
        let resp = ui.interact(rect, ui.id().with(id), Sense::click_and_drag());
        let sid = egui::Id::new(("mini-bar-scrub", id));
        let pid = egui::Id::new(("mini-bar-pending", id));
        let mut scrub: Option<f64> = ui.ctx().data_mut(|d| d.get_temp(sid));
        let mut pending: Option<(f64, std::time::Instant)> =
            ui.ctx().data_mut(|d| d.get_temp(pid));
        if interactive {
            if resp.is_pointer_button_down_on() {
                if let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    let ratio = ((p.x - rect.min.x) / rect.width().max(1.0)) as f64;
                    scrub = Some(ratio.clamp(0.0, 1.0) * duration.max(0.0));
                    // Live bars (volume) apply while dragging; the
                    // seek bar still waits for the release.
                    if live {
                        seek = scrub;
                    }
                }
            } else if let Some(v) = scrub.take() {
                seek = Some(v);
                pending = Some((v, std::time::Instant::now()));
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        } else if scrub.is_some() && !ui.ctx().input(|i| i.pointer.primary_down()) {
            seek = scrub.take();
            if let Some(v) = seek {
                pending = Some((v, std::time::Instant::now()));
            }
        }
        if let Some((t, when)) = pending {
            if (position - t).abs() < 0.25 || when.elapsed().as_secs_f32() > 0.75 {
                pending = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        let shown = scrub.or(pending.map(|(t, _)| t)).unwrap_or(position);
        ui.ctx().data_mut(|d| {
            if let Some(v) = scrub {
                d.insert_temp(sid, v);
            } else {
                d.remove::<f64>(sid);
            }
            if let Some(v) = pending {
                d.insert_temp(pid, v);
            } else {
                d.remove::<(f64, std::time::Instant)>(pid);
            }
        });
        let progress = if duration > 0.0 {
            (shown / duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        let painter = ui.painter();
        let rr = egui::CornerRadius::same((rect.height() * 0.5).round() as u8);
        painter.rect_filled(rect, rr, MINI_TRACK);
        if progress > 0.0 {
            // Minimum width of one diameter so the head always reads
            // as a round cap instead of a flat sliver.
            let w = (progress * rect.width()).max(rect.height());
            let fill =
                Rect::from_min_max(rect.min, Pos2::new(rect.min.x + w, rect.max.y));
            painter.rect_filled(fill, rr, MINI_FILL);
        }
        painter.rect_stroke(
            rect,
            rr,
            Stroke::new(1.0, MINI_BORDER),
            egui::StrokeKind::Inside,
        );
        seek
    }

    /// Spinner used by the strip while a waveform decodes.
    fn loading(&self, ui: &mut egui::Ui, rect: Rect, alpha: f32, s: f32) {
        let painter = ui.painter();
        let angle = ui.ctx().input(|i| i.time) as f32 * 360.0;
        let c = rect.center();
        for i in 0..SPINNER_SPOKES {
            let rad = (angle + i as f32 * (360.0 / SPINNER_SPOKES as f32)).to_radians();
            let (sin, cos) = rad.sin_cos();
            let a = ((i + 1) * 255 / SPINNER_SPOKES) as f32 / 255.0 * alpha;
            painter.line_segment(
                [
                    c + Vec2::new(cos, sin) * (SPINNER_R * 0.5 * s),
                    c + Vec2::new(cos, sin) * (SPINNER_R * s),
                ],
                Stroke::new(3.0 * s, Color32::from_white_alpha((a * 255.0) as u8)),
            );
        }
        ui.ctx().request_repaint();
    }
}
