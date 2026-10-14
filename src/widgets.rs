//! Interactive widgets, a port of AES_Lacrima's `WidgetControl` chrome
//! (move/resize in edit mode, marching-ants border, context menu) with
//! two contents: the `ClockCompositionControl` clock and the
//! `PlayerCompositionControl` turntable — a vinyl disc that spins the
//! active cover inside a circular progress ring (which doubles as a
//! seek bar), with a tonearm that toggles play/pause. The disc's
//! geometry is also exported as the ShaderToy `u_disc` occluder
//! (`ShaderDiscOccluderSync`).

use eframe::egui::{
    self, epaint, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Pos2, Rect,
    Response, Sense, Stroke, Vec2,
};
use std::time::{Duration, Instant};

pub const SEGOE_LIGHT_FAMILY: &str = "SegoeLight";

const EDIT_MARGIN: f32 = 8.0;
const GRIP_SIZE: f32 = 16.0;

// Clock widget defaults (virtual units): 250x250 at the content's
// top-left — Lacrima's ClockWidth/Height=250, Left/Top=NaN (=> 0,0).
const CLOCK_DEFAULT_SIZE: Vec2 = Vec2::new(250.0, 250.0);
const CLOCK_MIN: Vec2 = Vec2::new(90.0, 70.0);
const PLAYER_MIN: Vec2 = Vec2::new(140.0, 140.0);

fn clock_default(content: Rect, _frame: Rect, s: f32) -> Rect {
    Rect::from_min_size(content.min, CLOCK_DEFAULT_SIZE * s)
}

/// `MainContentWidgetLayout.Apply` for the turntable: proportional
/// size, disc center on the ShaderToy render origin — the center of
/// the whole window (`Atom` renders its rays at `0.5 * iResolution`,
/// and `u_disc` occluders track the disc center).
fn player_default(content: Rect, frame: Rect, s: f32) -> Rect {
    let w_v = content.width() / s;
    let h_v = content.height() / s;
    let pw = (w_v * 354.51 / 1092.0).max(180.0);
    let ph = (h_v * 372.04 / 769.0).max(200.0);
    // Use disc_layout on the REAL rect — disc_metrics has .max()
    // clamps so it isn't scale-linear; scaling virtual center_v by s
    // drifts the disc off the frame center by a few px.
    let size = Vec2::new(pw, ph) * s;
    let d = disc_layout(Rect::from_min_size(Pos2::ZERO, size));
    Rect::from_min_size(frame.center() - d.center.to_vec2(), size)
}

/// Translate `r` inside `bounds` without ever resizing: if the widget
/// is larger than the content on an axis it pins to that edge and
/// overflows (the painter clips it). A small window must not shrink
/// a fixed-size widget — the size would persist permanently.
fn fit_inside(rect: Rect, bounds: Rect) -> Rect {
    let (w, h) = (rect.width(), rect.height());
    let x = if w <= bounds.width() {
        rect.min.x.clamp(bounds.min.x, bounds.max.x - w)
    } else {
        bounds.min.x
    };
    let y = if h <= bounds.height() {
        rect.min.y.clamp(bounds.min.y, bounds.max.y - h)
    } else {
        bounds.min.y
    };
    Rect::from_min_size(Pos2::new(x, y), rect.size())
}

/// How one axis of a widget anchors as the window changes — the
/// placement intent, captured once and re-evaluated each frame.
/// `Min`/`Max` glue the rect to a CONTENT edge at an absolute gap
/// (a corner widget stays in the corner; content min moves with the
/// title bar); `Frac` keeps the CENTER at a fraction of the whole
/// FRAME, so e.g. the turntable disc stays on the shadertoy render
/// origin (frame center) through fullscreen.
#[derive(Clone, Copy)]
enum Anchor {
    Frac(f32),
    Min(f32),
    Max(f32),
}

/// An edge gap below this is treated as glued.
const EDGE_GLUE: f32 = 48.0;

#[allow(clippy::too_many_arguments)]
fn axis_anchor(
    min: f32,
    max: f32,
    content_min: f32,
    content_max: f32,
    frame_min: f32,
    frame_size: f32,
) -> Anchor {
    let dmin = min - content_min;
    let dmax = content_max - max;
    if dmin <= dmax && dmin < EDGE_GLUE {
        Anchor::Min(dmin)
    } else if dmax < dmin && dmax < EDGE_GLUE {
        Anchor::Max(dmax)
    } else {
        Anchor::Frac(((min + max) * 0.5 - frame_min) / frame_size.max(1.0))
    }
}

/// Evaluate an anchor to the axis' min coordinate for `size`.
#[allow(clippy::too_many_arguments)]
fn axis_pos(
    a: Anchor,
    size: f32,
    content_min: f32,
    content_max: f32,
    frame_min: f32,
    frame_size: f32,
) -> f32 {
    match a {
        Anchor::Min(g) => content_min + g,
        Anchor::Max(g) => content_max - g - size,
        Anchor::Frac(f) => frame_min + f * frame_size - size * 0.5,
    }
}

fn clamp_inside(rect: Rect, bounds: Rect) -> Rect {
    let mut r = rect;
    if r.width() > bounds.width() {
        r.min.x = bounds.min.x;
        r.max.x = bounds.max.x;
    } else {
        if r.min.x < bounds.min.x {
            r = r.translate(Vec2::new(bounds.min.x - r.min.x, 0.0));
        }
        if r.max.x > bounds.max.x {
            r = r.translate(Vec2::new(bounds.max.x - r.max.x, 0.0));
        }
    }
    if r.height() > bounds.height() {
        r.min.y = bounds.min.y;
        r.max.y = bounds.max.y;
    } else {
        if r.min.y < bounds.min.y {
            r = r.translate(Vec2::new(0.0, bounds.min.y - r.min.y));
        }
        if r.max.y > bounds.max.y {
            r = r.translate(Vec2::new(0.0, bounds.max.y - r.max.y));
        }
    }
    r
}

/// Shared `WidgetControl` chrome: rect init/restore, proportional
/// re-anchoring on scale/content resize, edit-mode move/resize handles.
pub struct WidgetFrame {
    pub rect: Rect,
    pub visible: bool,
    /// True while the user is moving/resizing the widget.
    pub dragging: bool,
    saved_offset: Option<Vec2>,
    saved_size: Option<Vec2>,
    /// Canonical placement — placement is recomputed from this every
    /// frame, so transient window sizes (startup races, fullscreen
    /// transitions) can never corrupt the position incrementally.
    anchor: Option<[Anchor; 2]>,
    last_scale: f32,
    /// Rect at drag start — `drag_delta` applies to this snapshot, not
    /// the live rect (which would compound the delta every frame).
    drag_from: Option<Rect>,
    default_rect: fn(Rect, Rect, f32) -> Rect,
    min_size: Vec2,
    id: &'static str,
}

/// Per-frame widget result: `ended` fires when a move/resize drag
/// finished (save trigger); `menu_resp` is the move hit-area so the
/// caller can attach the shared widget context menu (it can't be
/// built inside `ui` — the closure needs sibling widgets).
#[derive(Default)]
pub struct WidgetOut {
    pub ended: bool,
    pub menu_resp: Option<Response>,
}

impl WidgetFrame {
    fn new(
        saved: Option<(Vec2, Vec2, bool)>,
        default_rect: fn(Rect, Rect, f32) -> Rect,
        min_size: Vec2,
        id: &'static str,
    ) -> Self {
        Self {
            rect: Rect::NOTHING,
            visible: saved.map(|s| s.2).unwrap_or(true),
            dragging: false,
            saved_offset: saved.map(|s| s.0),
            saved_size: saved.map(|s| s.1),
            anchor: None,
            last_scale: 0.0,
            drag_from: None,
            default_rect,
            min_size,
            id,
        }
    }

    pub fn reset(&mut self, content: Rect, frame: Rect, s: f32) {
        self.rect = (self.default_rect)(content, frame, s);
        let r = self.rect;
        self.anchor = Some(self.anchor_of(r, content, frame));
    }

    /// Per-axis anchors for `r` — the placement intent captured once,
    /// then re-evaluated against every new geometry.
    fn anchor_of(&self, r: Rect, content: Rect, frame: Rect) -> [Anchor; 2] {
        [
            axis_anchor(
                r.min.x, r.max.x, content.min.x, content.max.x, frame.min.x, frame.width(),
            ),
            axis_anchor(
                r.min.y, r.max.y, content.min.y, content.max.y, frame.min.y, frame.height(),
            ),
        ]
    }

    /// Initialize the rect (or restore it) and re-place it as the
    /// window/scale changes. `frame` is the whole window — anchoring
    /// there (not `content`) keeps widgets at the same screen spot
    /// when the title bar collapses in fullscreen. Returns `visible`.
    fn begin(&mut self, content: Rect, frame: Rect, s: f32) -> bool {
        if !self.rect.is_finite() || !self.rect.is_positive() {
            // The anchor derives from the UNCLAMPED intent — on a
            // transient small first frame the display rect pins to an
            // edge, and anchoring THAT would glue the widget to the
            // corner permanently (the random drift on fresh builds).
            let (rect, intent) = match (self.saved_offset, self.saved_size) {
                (Some(off), Some(size)) => {
                    // Offset is the widget CENTER as a fraction of the
                    // whole window, so a widget keeps its screen spot
                    // in any mode; size is absolute virtual units —
                    // fullscreen must not inflate widgets.
                    let restored = Rect::from_center_size(
                        Pos2::new(
                            frame.min.x + off.x * frame.width(),
                            frame.min.y + off.y * frame.height(),
                        ),
                        size,
                    );
                    (fit_inside(restored, content.shrink(EDIT_MARGIN)), restored)
                }
                _ => {
                    let r = (self.default_rect)(content, frame, s);
                    (r, r)
                }
            };
            self.rect = rect;
            self.anchor = Some(self.anchor_of(intent, content, frame));
        }
        // The widget lives inside the ScalableDecorator: on a UI-scale
        // change the rect rescales once, then the anchor re-derives.
        if self.last_scale > 0.0 && (s - self.last_scale).abs() > 1e-6 {
            let k = s / self.last_scale;
            self.rect = Rect::from_center_size(self.rect.center(), self.rect.size() * k);
            let r = self.rect;
            self.anchor = Some(self.anchor_of(r, content, frame));
        }
        self.last_scale = s;
        // Placement is a pure function of (anchor, content, frame) —
        // recomputed every frame so any transient size converges back
        // to the same spot instead of chaining off a stale rect.
        // Skipped while dragging (chrome writes the rect; the anchor
        // re-derives on release).
        if let (Some(a), false) = (self.anchor, self.dragging) {
            let size = self.rect.size();
            let min = Pos2::new(
                axis_pos(a[0], size.x, content.min.x, content.max.x, frame.min.x, frame.width()),
                axis_pos(a[1], size.y, content.min.y, content.max.y, frame.min.y, frame.height()),
            );
            self.rect = fit_inside(
                Rect::from_min_size(min, size),
                content.shrink(EDIT_MARGIN),
            );
        }
        self.visible
    }

    /// Edit-mode chrome: marching-ants outline, move drag, resize grip.
    /// On release the anchor re-derives from the final rect so the
    /// dropped spot becomes the canonical placement.
    fn chrome(
        &mut self,
        ui: &mut egui::Ui,
        content: Rect,
        frame: Rect,
        elapsed: f32,
        s: f32,
    ) -> WidgetOut {
        let mut out = WidgetOut::default();
        let outer = self.rect.expand(EDIT_MARGIN);
        // Lacrima's focus rect: LightGray, {4,4} dashes, offset animated
        // so the dashes travel clockwise (~1px per 30ms).
        dashed_rect_stroke(
            ui.painter(),
            outer,
            8.0,
            Stroke::new(1.5, Color32::LIGHT_GRAY),
            4.0,
            4.0,
            elapsed * 33.0,
        );
        ui.ctx().request_repaint();

        let move_resp = ui.interact(
            outer,
            ui.id().with((self.id, "move")),
            Sense::click_and_drag(),
        );
        if move_resp.drag_started() {
            self.drag_from = Some(self.rect);
        }
        if move_resp.dragged() {
            // NOTE: `drag_delta` is per-frame motion in egui 0.36 —
            // the cumulative press-to-now offset is `total_drag_delta`.
            let from = self.drag_from.unwrap_or(self.rect);
            let moved = from.translate(move_resp.total_drag_delta().unwrap_or_default());
            let r = fit_inside(moved, content.shrink(EDIT_MARGIN));
            if r.is_positive() {
                self.rect = r;
            }
        }
        if move_resp.drag_stopped() {
            self.drag_from = None;
            let r = self.rect;
            self.anchor = Some(self.anchor_of(r, content, frame));
        }
        let move_dragged = move_resp.dragged();
        out.ended = move_resp.drag_stopped();
        move_resp.clone().on_hover_cursor(CursorIcon::Move);
        out.menu_resp = Some(move_resp);

        let grip = Rect::from_min_size(
            outer.max - Vec2::splat(GRIP_SIZE),
            Vec2::splat(GRIP_SIZE),
        );
        ui.painter().rect_filled(
            grip.shrink(3.0),
            CornerRadius::same(3),
            Color32::from_white_alpha(170),
        );
        let grip_resp = ui.interact(grip, ui.id().with((self.id, "resize")), Sense::drag());
        if grip_resp.drag_started() {
            self.drag_from = Some(self.rect);
        }
        if grip_resp.dragged() {
            let from = self.drag_from.unwrap_or(self.rect);
            let size = (from.size() + grip_resp.total_drag_delta().unwrap_or_default())
                .max(self.min_size * s);
            let resized = Rect::from_min_size(from.min, size);
            let r = clamp_inside(resized.expand(EDIT_MARGIN), content).shrink(EDIT_MARGIN);
            if r.is_positive() {
                self.rect = r;
            }
        }
        if grip_resp.drag_stopped() {
            self.drag_from = None;
            let r = self.rect;
            self.anchor = Some(self.anchor_of(r, content, frame));
        }
        let grip_dragged = grip_resp.dragged();
        out.ended = out.ended || grip_resp.drag_stopped();
        grip_resp.on_hover_cursor(CursorIcon::ResizeNwSe);
        self.dragging = move_dragged || grip_dragged;
        out
    }

    /// The persistable position: the anchor's center as a fraction of
    /// `frame` — evaluated from intent so a clamped or transient
    /// display rect can't leak into the save.
    pub fn anchor_offset(&self, content: Rect, frame: Rect) -> Vec2 {
        if let Some([ax, ay]) = self.anchor {
            let size = self.rect.size();
            let cx = axis_pos(
                ax, size.x, content.min.x, content.max.x, frame.min.x, frame.width(),
            ) + size.x * 0.5;
            let cy = axis_pos(
                ay, size.y, content.min.y, content.max.y, frame.min.y, frame.height(),
            ) + size.y * 0.5;
            Vec2::new(
                (cx - frame.min.x) / frame.width().max(1.0),
                (cy - frame.min.y) / frame.height().max(1.0),
            )
        } else {
            (self.rect.center() - frame.min) / frame.size()
        }
    }
}

pub struct ClockWidget {
    pub frame: WidgetFrame,
    have_font: bool,
}

impl std::ops::Deref for ClockWidget {
    type Target = WidgetFrame;
    fn deref(&self) -> &WidgetFrame {
        &self.frame
    }
}

impl std::ops::DerefMut for ClockWidget {
    fn deref_mut(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }
}

impl ClockWidget {
    /// `saved` is `(offset, size, visible)` from persisted settings;
    /// `offset` is relative to the content area's top-left corner.
    pub fn new(have_font: bool, saved: Option<(Vec2, Vec2, bool)>) -> Self {
        Self {
            frame: WidgetFrame::new(saved, clock_default, CLOCK_MIN, "clock"),
            have_font,
        }
    }

    /// Paint the clock and, when `edit_mode` is on, its move/resize chrome.
    /// `elapsed` drives the marching-ants border animation.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        content: Rect,
        frame: Rect,
        edit_mode: bool,
        elapsed: f32,
        s: f32,
    ) -> WidgetOut {
        if !self.frame.begin(content, frame, s) {
            return WidgetOut::default();
        }

        paint_clock(ui.painter(), self.frame.rect, self.have_font);

        if !edit_mode {
            self.frame.dragging = false;
            return WidgetOut::default();
        }
        self.frame.chrome(ui, content, frame, elapsed, s)
    }
}

// ---------------------------------------------------------------------------
// Turntable — `PlayerCompositionArmMetrics` + `PlayerCompositionVisualHandler`.
// ---------------------------------------------------------------------------

/// Tonearm angles (`PlayerCompositionArmMetrics`): parked off the disc /
/// lowered onto the disc while playing.
pub const ARM_REST_DEG: f32 = -64.0;
pub const ARM_PLAY_DEG: f32 = -108.0;
/// Disc spin rate while playing (deg/sec).
const DISC_RPM_SPEED: f32 = 30.0;

/// `GetDiscLayout` geometry — also the `u_disc` occluder source.
pub struct DiscLayout {
    pub center: Pos2,
    pub ring_radius: f32,
    #[allow(dead_code)]
    pub diameter: f32,
}

/// `PlayerCompositionArmMetrics.GetDiscLayout` on a widget-local size.
fn disc_metrics(w: f32, h: f32) -> DiscLayoutParts {
    let size = w.min(h);
    let reserved_right = 24.0f32.max(size * 0.2);
    let reserved_bottom = 18.0f32.max(size * 0.13);
    let avail_w = (w - reserved_right).max(1.0);
    let avail_h = (h - reserved_bottom).max(1.0);
    let diameter = 40.0f32.max(avail_w.min(avail_h));
    let ring_radius = 12.0f32.max(diameter * 0.5 - 13.5);
    DiscLayoutParts {
        center_v: Vec2::new(avail_w * 0.5, avail_h * 0.5),
        ring_radius,
        diameter,
    }
}

struct DiscLayoutParts {
    center_v: Vec2,
    ring_radius: f32,
    diameter: f32,
}

/// `GetDiscLayout` on a screen-space widget rect.
pub fn disc_layout(rect: Rect) -> DiscLayout {
    let p = disc_metrics(rect.width(), rect.height());
    DiscLayout {
        center: rect.min + p.center_v,
        ring_radius: p.ring_radius,
        diameter: p.diameter,
    }
}

struct ArmLayout {
    pivot: Pos2,
    elbow: Pos2,
    headshell_start: Pos2,
    headshell_end: Pos2,
    counterweight_end: Pos2,
    tube: f32,
    headshell_t: f32,
    pivot_r: f32,
    counterweight_w: f32,
    counterweight_len: f32,
}

/// `PlayerCompositionArmMetrics.GetLayout` for a widget rect + angle.
fn arm_layout(rect: Rect, angle_deg: f32) -> ArmLayout {
    let d = disc_layout(rect);
    let size = d.diameter;
    let r = d.ring_radius;
    let c = d.center;

    let pivot = Pos2::new(c.x + r * 0.82, c.y + r * 0.82);
    let play_elbow = Pos2::new(c.x + r * 0.7, c.y + r * 0.46);
    let elbow_len = (size * 0.26).max(pivot.distance(play_elbow));
    let headshell_len = size * 0.085;
    let counterweight_len = size * 0.075;
    let tube = (size * 0.018).max(5.0);
    let headshell_t = (size * 0.022).max(6.5);
    let pivot_r = (size * 0.05).max(13.0);
    let counterweight_w = (size * 0.022).max(6.0);

    let rad = angle_deg.to_radians();
    let (sn, cs) = rad.sin_cos();
    let elbow = Pos2::new(pivot.x + cs * elbow_len, pivot.y + sn * elbow_len);

    let hrad = (angle_deg + 10.0).to_radians();
    let (hsn, hcs) = hrad.sin_cos();
    let head_center = Pos2::new(
        elbow.x + hcs * (headshell_len * 1.45),
        elbow.y + hsn * (headshell_len * 1.45),
    );
    let headshell_start = Pos2::new(
        head_center.x - hcs * (headshell_len * 0.5),
        head_center.y - hsn * (headshell_len * 0.5),
    );
    let headshell_end = Pos2::new(
        head_center.x + hcs * (headshell_len * 0.5),
        head_center.y + hsn * (headshell_len * 0.5),
    );
    let counterweight_end = Pos2::new(
        pivot.x - cs * counterweight_len,
        pivot.y - sn * counterweight_len,
    );

    ArmLayout {
        pivot,
        elbow,
        headshell_start,
        headshell_end,
        counterweight_end,
        tube,
        headshell_t,
        pivot_r,
        counterweight_w,
        counterweight_len,
    }
}

fn dist_seg(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let d = b - a;
    let len2 = d.x * d.x + d.y * d.y;
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = (((p.x - a.x) * d.x + (p.y - a.y) * d.y) / len2).clamp(0.0, 1.0);
    p.distance(a + d * t)
}

/// `IsPointOnArm` — evaluated at the rest/play extremes like Lacrima,
/// not the animated in-between angle.
fn point_on_arm(rect: Rect, p: Pos2, is_playing: bool) -> bool {
    let l = arm_layout(rect, if is_playing { ARM_PLAY_DEG } else { ARM_REST_DEG });
    p.distance(l.pivot) <= l.pivot_r * 1.2
        || dist_seg(p, l.pivot, l.elbow) <= l.tube
        || dist_seg(p, l.headshell_start, l.headshell_end) <= l.headshell_t
        || dist_seg(p, l.counterweight_end, l.pivot) <= l.counterweight_w.max(l.tube * 0.95)
}

/// The turntable widget (`PlayerCompositionControl`): spinning cover
/// disc, circular progress ring that scrubs the play position, and a
/// tonearm that toggles play/pause on click.
pub struct PlayerWidget {
    pub frame: WidgetFrame,
    rotation_deg: f32,
    arm_deg: f32,
    last_anim: Option<Instant>,
    /// Position preview while ring-scrubbing (the real position only
    /// catches up when a throttled seek lands).
    preview_pos: Option<f64>,
    last_seek: Instant,
    seek_active: bool,
}

impl std::ops::Deref for PlayerWidget {
    type Target = WidgetFrame;
    fn deref(&self) -> &WidgetFrame {
        &self.frame
    }
}

impl std::ops::DerefMut for PlayerWidget {
    fn deref_mut(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }
}

/// Actions the widget produced this frame for the caller to apply.
#[derive(Default)]
pub struct PlayerOut {
    pub ended: bool,
    pub menu_resp: Option<Response>,
    /// Seek request in seconds (throttled during a scrub, final on
    /// release — `SetPositionCommand` semantics).
    pub seek: Option<f64>,
    /// The tonearm was clicked — toggle play/pause.
    pub toggle_play: bool,
}

impl PlayerWidget {
    pub fn new(saved: Option<(Vec2, Vec2, bool)>) -> Self {
        Self {
            frame: WidgetFrame::new(saved, player_default, PLAYER_MIN, "player"),
            rotation_deg: 0.0,
            arm_deg: ARM_REST_DEG,
            last_anim: None,
            preview_pos: None,
            last_seek: Instant::now(),
            seek_active: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        content: Rect,
        frame: Rect,
        edit_mode: bool,
        interactive: bool,
        elapsed: f32,
        s: f32,
        cover: Option<&egui::TextureHandle>,
        position: f64,
        duration: f64,
        is_playing: bool,
    ) -> PlayerOut {
        let mut out = PlayerOut::default();
        if !self.frame.begin(content, frame, s) {
            return out;
        }
        let ctx = ui.ctx().clone();
        let rect = self.frame.rect;

        // Animation: disc spins at 30 deg/s while playing; the tonearm
        // eases between rest and play with Lacrima's exp(-dt*9) curve.
        let now = Instant::now();
        let dt = self
            .last_anim
            .map(|t| now.duration_since(t).as_secs_f32())
            .unwrap_or(1.0 / 60.0)
            .min(0.1);
        self.last_anim = Some(now);
        let mut animating = false;
        if is_playing {
            self.rotation_deg = (self.rotation_deg + dt * DISC_RPM_SPEED).rem_euclid(360.0);
            animating = true;
        }
        let target_arm = if is_playing { ARM_PLAY_DEG } else { ARM_REST_DEG };
        let d_arm = target_arm - self.arm_deg;
        if d_arm.abs() > 0.15 {
            self.arm_deg += d_arm * (1.0 - (-dt * 9.0).exp());
            animating = true;
        } else {
            self.arm_deg = target_arm;
        }
        if animating {
            ctx.request_repaint();
        }

        // Input (raw pointer like the carousel — the disc hit areas are
        // geometric, not widget-rect based).
        let d = disc_layout(rect);
        if interactive && !edit_mode {
            let pressed = ctx.input(|i| i.pointer.primary_pressed());
            let down = ctx.input(|i| i.pointer.primary_down());
            let released = ctx.input(|i| i.pointer.primary_released());
            let pos = ctx
                .input(|i| i.pointer.interact_pos())
                .or_else(|| ctx.input(|i| i.pointer.latest_pos()));
            if pressed {
                if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
                    if rect.contains(p) {
                        if point_on_arm(rect, p, is_playing) {
                            // Lacrima toggles on press.
                            out.toggle_play = true;
                        } else if on_ring(&d, p) && duration > 0.0 {
                            self.seek_active = true;
                            let t = seek_seconds(&d, p, duration);
                            self.preview_pos = Some(t);
                            out.seek = Some(t);
                            self.last_seek = Instant::now();
                        }
                    }
                }
            }
            if self.seek_active {
                if down {
                    if let Some(p) = pos {
                        let t = seek_seconds(&d, p, duration);
                        self.preview_pos = Some(t);
                        // 300ms throttle during the drag, same as
                        // Lacrima's _lastSeekTime gate.
                        if self.last_seek.elapsed() > Duration::from_millis(300) {
                            out.seek = Some(t);
                            self.last_seek = Instant::now();
                        }
                    }
                } else {
                    // Final update on release.
                    if released {
                        out.seek = self.preview_pos.take();
                    } else {
                        self.preview_pos = None;
                    }
                    self.seek_active = false;
                }
            }
            if let Some(p) = pos {
                if rect.contains(p) && (on_ring(&d, p) || point_on_arm(rect, p, is_playing)) {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                }
            }
        } else {
            self.seek_active = false;
            self.preview_pos = None;
        }

        let pos = self.preview_pos.unwrap_or(position);
        paint_player(
            ui.painter(),
            rect,
            &d,
            arm_layout(rect, self.arm_deg),
            cover,
            self.rotation_deg,
            pos,
            duration,
        );

        if edit_mode {
            let chrome = self.frame.chrome(ui, content, frame, elapsed, s);
            out.ended = chrome.ended;
            out.menu_resp = chrome.menu_resp;
        } else {
            self.frame.dragging = false;
        }
        out
    }
}

/// `IsOnRing`: 25px tolerance around the progress ring.
fn on_ring(d: &DiscLayout, p: Pos2) -> bool {
    (p.distance(d.center) - d.ring_radius).abs() < 25.0
}

/// `CalculateAndSeek`: pointer angle around the disc (12 o'clock = 0)
/// mapped to a track position in seconds.
fn seek_seconds(d: &DiscLayout, p: Pos2, duration: f64) -> f64 {
    let v = p - d.center;
    let deg = v.y.atan2(v.x).to_degrees();
    let progress = (deg + 450.0).rem_euclid(360.0) / 360.0;
    progress as f64 * duration
}

/// The cover drawn inside the ring as a rotating annulus-clipped,
/// cover-fit image (`DrawCircleBackground` + `ShowDiscCenter`: the hub
/// disc is cut out so the center shows the background through the hub
/// tint, and the hole stays fully see-through). `r_outer` is tucked
/// under the progress ring so the ring sits on the disc edge.
fn cover_mesh(
    tex: egui::TextureId,
    img: Vec2,
    center: Pos2,
    r_inner: f32,
    r_outer: f32,
    rotation: f32,
    alpha: f32,
) -> epaint::Mesh {
    const SEGS: usize = 96;
    let dest = r_outer * 2.0;
    let scale = (dest / img.x.max(1.0)).max(dest / img.y.max(1.0));
    // Half-extents of the sampled src rect in uv units (cover fit).
    let huv = Vec2::new(dest / scale / img.x / 2.0, dest / scale / img.y / 2.0);
    let frac_in = r_inner / r_outer;
    let (sn, cs) = rotation.to_radians().sin_cos();
    let col = Color32::from_white_alpha((alpha * 255.0) as u8);
    let mut mesh = epaint::Mesh {
        texture_id: tex,
        ..Default::default()
    };
    for i in 0..=SEGS {
        let a = std::f32::consts::TAU * i as f32 / SEGS as f32;
        let dir = Vec2::new(a.cos(), a.sin());
        // Sample the image rotated by -rotation so it appears rotated
        // +rotation on screen (Skia RotateDegrees).
        let rd = Vec2::new(dir.x * cs + dir.y * sn, -dir.x * sn + dir.y * cs);
        mesh.vertices.push(epaint::Vertex {
            pos: center + dir * r_inner,
            uv: Pos2::new(
                0.5 + rd.x * huv.x * frac_in,
                0.5 + rd.y * huv.y * frac_in,
            ),
            color: col,
        });
        mesh.vertices.push(epaint::Vertex {
            pos: center + dir * r_outer,
            uv: Pos2::new(0.5 + rd.x * huv.x, 0.5 + rd.y * huv.y),
            color: col,
        });
    }
    for i in 0..SEGS as u32 {
        let (a, b) = (2 * i, 2 * i + 1);
        mesh.indices.extend_from_slice(&[a, b, a + 2]);
        mesh.indices.extend_from_slice(&[b, b + 2, a + 2]);
    }
    mesh
}

fn arc_points(center: Pos2, r: f32, start_deg: f32, sweep_deg: f32) -> Vec<Pos2> {
    let steps = (sweep_deg.abs() / 4.0).ceil().max(1.0) as usize;
    (0..=steps)
        .map(|i| {
            let a = (start_deg + sweep_deg * i as f32 / steps as f32).to_radians();
            center + Vec2::new(r * a.cos(), r * a.sin())
        })
        .collect()
}

fn midpoint(a: Pos2, b: Pos2) -> Pos2 {
    Pos2::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5)
}

fn norm(v: Vec2) -> Vec2 {
    let len = (v.x * v.x + v.y * v.y).sqrt().max(1.0);
    v / len
}

/// `PlayerCompositionVisualHandler` draw order: disc (cover), hub,
/// progress ring + thumb, tonearm.
fn paint_player(
    painter: &egui::Painter,
    rect: Rect,
    d: &DiscLayout,
    arm: ArmLayout,
    cover: Option<&egui::TextureHandle>,
    rotation: f32,
    position: f64,
    duration: f64,
) {
    let _ = rect;
    let r = d.ring_radius;
    // Ring widths scale with the disc so the ring reads as the thin
    // edge cap from Lacrima at any widget size.
    let bg_w = (r * 0.022).clamp(2.0, 6.0);
    let arc_w = (r * 0.028).clamp(2.5, 7.0);
    let thumb_r = (r * 0.055).clamp(4.0, 8.0);
    // Disc center (ShowDiscCenter): translucent hub annulus + rims; the
    // center hole stays transparent, showing the background through.
    let hub_r = r * 0.35;
    let hole_r = r * 0.12;
    // Spinning cover disc (CircleBackgroundOpacity=0.9) as an annulus —
    // Lacrima clips the hub out of the bitmap; the disc border sits on
    // the progress ring's centerline so the ring straddles the edge.
    if let Some(tex) = cover {
        let mesh = cover_mesh(
            tex.id(),
            tex.size_vec2(),
            d.center,
            hub_r,
            r,
            rotation,
            0.9,
        );
        painter.add(egui::Shape::mesh(mesh));
    }
    painter.circle_stroke(
        d.center,
        hole_r,
        Stroke::new(hub_r - hole_r, Color32::from_white_alpha(30)),
    );
    painter.circle_stroke(d.center, hub_r, Stroke::new(0.5, Color32::from_black_alpha(40)));
    painter.circle_stroke(d.center, hole_r, Stroke::new(0.5, Color32::from_black_alpha(40)));
    painter.circle_stroke(
        d.center,
        hub_r - 2.0,
        Stroke::new(1.0, Color32::from_white_alpha(80)),
    );
    painter.circle_stroke(
        d.center,
        hole_r + 2.0,
        Stroke::new(1.0, Color32::from_white_alpha(80)),
    );

    // Progress ring: translucent full circle + white arc from 12
    // o'clock with the thumb dot at the sweep end. `circle_stroke`
    // paints outside the given radius — offset by bg_w/2 so the track
    // centers on the disc edge like the arc's middle stroke.
    painter.circle_stroke(
        d.center,
        r - bg_w * 0.5,
        Stroke::new(bg_w, Color32::from_white_alpha(160)),
    );
    if duration > 0.0 {
        let frac = (position / duration) as f32;
        if frac > 0.0 {
            let sweep = frac * 360.0;
            let pts = arc_points(d.center, r, -90.0, sweep);
            painter.add(egui::Shape::line(pts, Stroke::new(arc_w, Color32::WHITE)));
            let a = (-90.0 + sweep).to_radians();
            painter.circle_filled(
                d.center + Vec2::new(r * a.cos(), r * a.sin()),
                thumb_r,
                Color32::WHITE,
            );
        }
    }

    paint_arm(painter, &arm);
}

/// `DrawArm`: white tube + headshell + counterweight + pivot.
fn paint_arm(painter: &egui::Painter, l: &ArmLayout) {
    let white = Color32::WHITE;
    let tube = Stroke::new(l.tube, white);

    painter.line_segment([l.pivot, l.elbow], tube);
    // Round the exposed tube ends like Skia's StrokeCap.Round.
    painter.circle_filled(l.pivot, l.tube * 0.5, white);
    let hs_mid = midpoint(l.headshell_start, l.headshell_end);
    painter.line_segment([l.elbow, hs_mid], tube);
    painter.circle_filled(hs_mid, l.tube * 0.5, white);
    painter.circle_filled(l.elbow, (l.tube * 0.42).max(1.6), white);

    // Headshell quad aligned with its axis.
    let hd = norm(l.headshell_end - l.headshell_start);
    let hp = Vec2::new(-hd.y, hd.x);
    let hl = (l.headshell_t * 2.2).max(10.0);
    let hw = (l.headshell_t * 1.4).max(5.8);
    painter.add(egui::Shape::convex_polygon(
        vec![
            hs_mid - hd * hl - hp * hw,
            hs_mid + hd * hl - hp * hw,
            hs_mid + hd * hl + hp * hw,
            hs_mid - hd * hl + hp * hw,
        ],
        white,
        Stroke::NONE,
    ));

    // Counterweight quad behind the pivot.
    let cm = midpoint(l.pivot, l.counterweight_end);
    let cd = norm(l.counterweight_end - l.pivot);
    let cp = Vec2::new(-cd.y, cd.x);
    let cl = (l.counterweight_len * 0.45).max(3.0);
    let cw = l.counterweight_w * 0.5;
    painter.add(egui::Shape::convex_polygon(
        vec![
            cm - cd * cl - cp * cw,
            cm + cd * cl - cp * cw,
            cm + cd * cl + cp * cw,
            cm - cd * cl + cp * cw,
        ],
        white,
        Stroke::NONE,
    ));

    painter.circle_stroke(l.pivot, l.pivot_r, Stroke::new((l.pivot_r * 0.16).max(3.0), white));
    painter.circle_filled(l.pivot, l.pivot_r * 0.48, white);
}

fn push_arc(pts: &mut Vec<Pos2>, center: Pos2, r: f32, from_deg: f32, to_deg: f32, steps: usize) {
    for i in 1..=steps {
        let a = (from_deg + (to_deg - from_deg) * i as f32 / steps as f32).to_radians();
        pts.push(Pos2::new(center.x + r * a.cos(), center.y + r * a.sin()));
    }
}

/// Stroke a rounded rectangle with a dash/gap pattern; `phase` shifts the
/// pattern along the outline so increasing it moves the dashes clockwise.
fn dashed_rect_stroke(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    stroke: Stroke,
    dash: f32,
    gap: f32,
    phase: f32,
) {
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let steps = 8;
    // Clockwise outline, starting at the top-left corner end.
    let mut pts: Vec<Pos2> = vec![Pos2::new(rect.min.x + r, rect.min.y)];
    pts.push(Pos2::new(rect.max.x - r, rect.min.y));
    push_arc(&mut pts, Pos2::new(rect.max.x - r, rect.min.y + r), r, -90.0, 0.0, steps);
    pts.push(Pos2::new(rect.max.x, rect.max.y - r));
    push_arc(&mut pts, Pos2::new(rect.max.x - r, rect.max.y - r), r, 0.0, 90.0, steps);
    pts.push(Pos2::new(rect.min.x + r, rect.max.y));
    push_arc(&mut pts, Pos2::new(rect.min.x + r, rect.max.y - r), r, 90.0, 180.0, steps);
    pts.push(Pos2::new(rect.min.x, rect.min.y + r));
    push_arc(&mut pts, Pos2::new(rect.min.x + r, rect.min.y + r), r, 180.0, 270.0, steps);

    let period = dash + gap;
    // Keep the phase small — huge values lose f32 precision and can make
    // `rem_euclid` return exactly `period`, which would loop forever below.
    let phase = phase.rem_euclid(period);
    let mut dist = 0.0f32;
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        let len = a.distance(b);
        if !(len > 0.0) {
            continue;
        }
        let dir = (b - a) / len;
        // Guaranteed forward progress: an advance smaller than the
        // f32 precision at this magnitude is a no-op and would loop
        // forever (the frame freezes inside this while).
        let eps = (len * 1e-6).max(1e-4);
        let mut t = 0.0f32;
        let mut steps = 0u32;
        while t < len && steps < 4096 {
            steps += 1;
            // `rem_euclid` may round up to exactly `period`; that's the
            // same as offset 0 (dash start), so fold it back.
            let off = (dist + t - phase).rem_euclid(period);
            let off = if off >= period { 0.0 } else { off };
            if off < dash {
                let seg = (dash - off).min(len - t);
                painter.line_segment([a + dir * t, a + dir * (t + seg)], stroke);
                t += seg.max(eps);
            } else {
                t += ((period - off).min(len - t)).max(eps);
            }
        }
        dist += len;
    }
}

fn paint_clock(painter: &egui::Painter, rect: Rect, have_font: bool) {
    let now = chrono::Local::now();
    let time_str = now.format("%H : %M").to_string();
    let date_str = now.format("%d %b | %a").to_string().to_uppercase();

    let family = if have_font {
        FontFamily::Name(SEGOE_LIGHT_FAMILY.into())
    } else {
        FontFamily::Proportional
    };
    let size = rect.width().min(rect.height());
    // Round to whole pixels: every fractional font size is a separate
    // glyph rasterization in egui's font atlas, and continuous resizing
    // would thrash it.
    let time_size = (size / 3.6).clamp(20.0, 64.0).round();
    let date_size = (time_size * 0.36).clamp(11.0, 22.0).round();

    let text_color = Color32::WHITE;
    let date_color = Color32::from_rgb(200, 200, 200);

    let time_font = FontId::new(time_size, family.clone());
    let date_font = FontId::new(date_size, family);
    let time_galley = painter.layout_no_wrap(time_str, time_font, text_color);
    let date_galley = painter.layout_no_wrap(date_str, date_font, date_color);

    let gap = 4.0 + size * 0.03;
    let block_h = time_galley.rect.height() + gap + 1.5 + gap + date_galley.rect.height();
    let cx = rect.center().x;
    let mut y = rect.center().y - block_h / 2.0;

    painter.galley(
        Pos2::new(cx - time_galley.rect.width() / 2.0, y),
        time_galley.clone(),
        text_color,
    );
    y += time_galley.rect.height() + gap + 0.75;

    // Separator: slightly wider than the date text (x1.1), like the original.
    let sep_w = date_galley.rect.width() * 1.1;
    painter.line_segment(
        [
            Pos2::new(cx - sep_w / 2.0, y),
            Pos2::new(cx + sep_w / 2.0, y),
        ],
        Stroke::new(1.5, text_color),
    );
    y += 0.75 + gap;

    painter.galley(
        Pos2::new(cx - date_galley.rect.width() / 2.0, y),
        date_galley,
        date_color,
    );
}
