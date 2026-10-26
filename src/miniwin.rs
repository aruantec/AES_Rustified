//! Wayland-only floating mini window.
//!
//! Native Wayland gives a toplevel no position control — the
//! compositor alone decides where a window sits and resizes stay
//! anchored where they were. What the protocol DOES allow is
//! `wl_subsurface`: a child surface whose offset inside the parent
//! is fully app-controlled and composites anywhere, even outside
//! the parent's bounds. The mini player therefore renders to a
//! subsurface of the main window while the main window ghosts
//! itself to a transparent frame and narrows its input region to
//! the mini's rect. The mini's own input region is empty, so
//! pointer events fall through to the parent surface, reach
//! winit/egui as ordinary window input, and are translated and
//! forwarded to a private egui::Context that paints the whole mini
//! chrome + view. That gives the two shell modes real, separate
//! geometry — the "two windows" the compositor won't grant — with
//! position kept in parent-surface coordinates, so a moved parent
//! drags the parked mini with it.
#![cfg(all(unix, not(target_os = "macos")))]

use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;
use std::sync::OnceLock;


use eframe::egui::{self, Pos2, Rect, Vec2};
use eframe::egui_wgpu;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use wayland_sys::client::*;
use wayland_sys::common::wl_interface;

use crate::ResizeDirection;

// Shared textures live in `crate::shared_tex` — queued deltas are
// injected into this context's `textures_delta` before upload so
// main-context covers/icons resolve in the mini renderer too.

// ----------------------------------------------------------------
// Wayland plumbing — raw libwayland calls on winit's own connection.
// wayland-client-rs can't wrap a foreign wl_display, so the protocol
// requests go through wl_proxy_marshal directly (the same calls the
// C headers inline). wayland-sys is built with `dlopen`, so the
// functions live behind wayland_client_handle() (ffi_dispatch! is
// unusable here — its dlopen cfg refers to the calling crate's
// features). Every object is just a *mut wl_proxy. All proxies land
// on the display's DEFAULT event queue — winit's own objects live on
// its private queue, so wl_display_roundtrip never swallows winit
// traffic.
// ----------------------------------------------------------------

/// Call a libwayland function through the dlopen'd handle.
macro_rules! wl {
    ($f:ident ( $($a:expr),* $(,)? )) => {
        unsafe { (wayland_client_handle().$f)($($a),*) }
    };
}

/// `wl_*_interface` globals — link-time symbols don't exist under
/// dlopen, so resolve them from the already-loaded client library.
fn wl_iface(sym: &'static CStr) -> Option<*const wl_interface> {
    // Stored as usize: a static OnceLock can't hold a raw pointer
    // (not Send/Sync). The handle itself is never closed.
    static LIB: OnceLock<usize> = OnceLock::new();
    let lib = *LIB.get_or_init(|| unsafe {
        let h = libc::dlopen(
            c"libwayland-client.so.0".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_NOLOAD,
        );
        let h = if h.is_null() {
            // Not loaded under that soname (paranoia) — load it.
            libc::dlopen(c"libwayland-client.so.0".as_ptr(), libc::RTLD_NOW)
        } else {
            h
        };
        h as usize
    });
    if lib == 0 {
        return None;
    }
    let p = unsafe { libc::dlsym(lib as *mut c_void, sym.as_ptr()) };
    if p.is_null() {
        None
    } else {
        Some(p.cast())
    }
}

#[derive(Default)]
struct Globals {
    compositor: *mut wl_proxy,
    subcompositor: *mut wl_proxy,
}

unsafe extern "C" fn on_global(
    data: *mut c_void,
    registry: *mut wl_proxy,
    name: u32,
    interface: *const c_char,
    version: u32,
) {
    let g = unsafe { &mut *(data as *mut Globals) };
    let Ok(iface) = (unsafe { CStr::from_ptr(interface) }).to_str() else {
        return;
    };
    match iface {
        "wl_compositor" => {
            if let Some(i) = wl_iface(c"wl_compositor_interface") {
                g.compositor = wl!(wl_proxy_marshal_constructor_versioned(
                    registry, 0, i, version.min(6),
                    name, c"wl_compositor".as_ptr(), version.min(6),
                ));
            }
        }
        "wl_subcompositor" => {
            if let Some(i) = wl_iface(c"wl_subcompositor_interface") {
                g.subcompositor = wl!(wl_proxy_marshal_constructor_versioned(
                    registry, 0, i, 1,
                    name, c"wl_subcompositor".as_ptr(), 1,
                ));
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn on_global_remove(
    _data: *mut c_void,
    _registry: *mut wl_proxy,
    _name: u32,
) {
}

#[repr(C)]
struct RegistryListener {
    global: unsafe extern "C" fn(*mut c_void, *mut wl_proxy, u32, *const c_char, u32),
    global_remove: unsafe extern "C" fn(*mut c_void, *mut wl_proxy, u32),
}

static REGISTRY_LISTENER: RegistryListener = RegistryListener {
    global: on_global,
    global_remove: on_global_remove,
};

/// Wgpu surface target over a raw wayland surface — the subsurface
/// is just a wl_surface, so wgpu presents to it exactly like it does
/// to a toplevel.
struct WlWin {
    display: NonNull<c_void>,
    surface: NonNull<c_void>,
}

impl HasWindowHandle for WlWin {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = WaylandWindowHandle::new(self.surface);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Wayland(h)) })
    }
}

impl HasDisplayHandle for WlWin {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let h = WaylandDisplayHandle::new(self.display);
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Wayland(h)) })
    }
}

// The pointers are only read while wgpu creates the surface; the
// wayland objects themselves are internally synchronized.
unsafe impl Send for WlWin {}
unsafe impl Sync for WlWin {}

// ----------------------------------------------------------------
// The floating window
// ----------------------------------------------------------------

/// Integer buffer scale for the subsurface. GNOME runs fractional
/// output scale (2.667 here); wl_surface.set_buffer_scale is integer
/// only, so the mini renders at 3x — slightly denser than the main
/// window but fully crisp.
const BUF_SCALE: i32 = 3;

/// Buffer px must be an exact multiple of BUF_SCALE — a fractional
/// logical size makes `size*scale` non-integral, which is a Wayland
/// protocol error that kills the client. Snapping to whole points
/// keeps `size*BUF_SCALE` exact (and sub-point sizing is invisible).
fn snap_size(mut size: Vec2) -> Vec2 {
    size.x = size.x.round().max(1.0);
    size.y = size.y.round().max(1.0);
    size
}

/// Width of the resize grip band around the mini's edge, in points.
const GRIP: f32 = 8.0;

pub(crate) struct MiniWin {
    // wayland — all protocol objects are opaque proxies
    parent: *mut wl_proxy,      // parent's wl_surface
    registry: *mut wl_proxy,
    compositor: *mut wl_proxy,
    surface: *mut wl_proxy,     // child wl_surface
    subsurface: *mut wl_proxy,
    region_iface: *const wl_interface,
    // egui + gpu
    ctx: egui::Context,
    main_ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
    device: wgpu::Device,
    queue: wgpu::Queue,
    wgpu_surface: wgpu::Surface<'static>,
    cfg: wgpu::SurfaceConfiguration,
    // geometry, parent-surface-local points
    pub pos: Pos2,
    pub size: Vec2,
    active: bool,
    dirty_save: bool,
    // input router
    drag: Option<Pos2>,
    resize: Option<(ResizeDirection, Pos2, Pos2, Vec2)>,
    hover: Option<Pos2>,
    swallow_release: bool,
    /// main-context texture id → mini-context texture id. Both
    /// managers allocate Managed ids from zero, so ids must be
    /// translated (not just copied) or they'd collide.
    tex_map: std::collections::HashMap<egui::TextureId, egui::TextureId>,
    /// Center once `monitor_size` becomes known — activation can run
    /// before the viewport reports it, and a saved position must not
    /// be overwritten, so this stays a separate flag.
    pending_center: bool,
}

impl MiniWin {
    /// `None` when the backend isn't Wayland or the plumbing fails —
    /// the caller then keeps the legacy in-place resize path.
    pub fn new<W: HasWindowHandle + HasDisplayHandle>(
        window: &W,
        rs: &egui_wgpu::RenderState,
        main_ctx: egui::Context,
        size: Vec2,
        pos: Pos2,
    ) -> Option<Self> {
        let size = snap_size(size);
        let (RawDisplayHandle::Wayland(dh), RawWindowHandle::Wayland(wh)) = (
            window.display_handle().ok()?.as_raw(),
            window.window_handle().ok()?.as_raw(),
        ) else {
            return None;
        };
        let display = dh.display.as_ptr() as *mut wl_display;
        let parent = wh.surface.as_ptr() as *mut wl_proxy;

        {
            let registry_iface = wl_iface(c"wl_registry_interface")?;
            let surface_iface = wl_iface(c"wl_surface_interface")?;
            let subsurface_iface = wl_iface(c"wl_subsurface_interface")?;
            let region_iface = wl_iface(c"wl_region_interface")?;

            let mut globals = Globals::default();
            let registry = wl!(wl_proxy_marshal_constructor(
                display.cast(), 1 /* get_registry */, registry_iface,
                std::ptr::null::<c_void>()
            ));
            if registry.is_null() {
                return None;
            }
            wl!(wl_proxy_add_listener(
                registry,
                &REGISTRY_LISTENER as *const _ as *mut extern "C" fn(),
                &mut globals as *mut Globals as *mut c_void,
            ));
            // Dispatch the default queue once so the globals land.
            wl!(wl_display_roundtrip(display));
            if globals.compositor.is_null() || globals.subcompositor.is_null() {
                wl!(wl_proxy_destroy(registry));
                return None;
            }
            let surface = wl!(wl_proxy_marshal_constructor(
                globals.compositor,
                0, // wl_compositor.create_surface
                surface_iface,
                std::ptr::null::<c_void>(),
            ));
            if surface.is_null() {
                wl!(wl_proxy_destroy(registry));
                return None;
            }
            let subsurface = wl!(wl_proxy_marshal_constructor(
                globals.subcompositor,
                1, // wl_subcompositor.get_subsurface
                subsurface_iface,
                std::ptr::null::<c_void>(), // new_id slot
                surface,
                parent,
            ));
            if subsurface.is_null() {
                wl!(wl_proxy_destroy(registry));
                return None;
            }
            // Own commit schedule — don't wait on parent commits.
            wl!(wl_proxy_marshal(subsurface, 5 /* set_desync */));
            // Present at BUF_SCALE density.
            wl!(wl_proxy_marshal(surface, 8 /* set_buffer_scale */, BUF_SCALE));
            // Empty input region — events fall through to the parent.
            let empty = wl!(wl_proxy_marshal_constructor(
                globals.compositor,
                1, // wl_compositor.create_region
                region_iface,
                std::ptr::null::<c_void>(),
            ));
            wl!(wl_proxy_marshal(surface, 5 /* set_input_region */, empty));
            wl!(wl_proxy_marshal(surface, 6 /* commit */));

            let handle = WlWin {
                display: NonNull::new(display.cast()).unwrap(),
                surface: NonNull::new(surface.cast()).unwrap(),
            };
            let wgpu_surface = rs.instance.create_surface(handle).ok()?;
            let caps = wgpu_surface.get_capabilities(&rs.adapter);
            // Must match the main window's surface format — the
            // app's paint-callback pipelines (shadertoy) are built
            // against rs.target_format.
            let format = if caps.formats.contains(&rs.target_format) {
                rs.target_format
            } else {
                return None;
            };
            let alpha = caps
                .alpha_modes
                .iter()
                .copied()
                .find(|m| matches!(m, wgpu::CompositeAlphaMode::PreMultiplied))
                .unwrap_or(caps.alpha_modes[0]);
            let (pw, ph) = (
                size.x as u32 * BUF_SCALE as u32,
                size.y as u32 * BUF_SCALE as u32,
            );
            let cfg = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC,
                format,
                color_space: wgpu::SurfaceColorSpace::Auto,
                width: pw.max(1),
                height: ph.max(1),
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: alpha,
                view_formats: vec![],
            };
            wgpu_surface.configure(&rs.device, &cfg);

            // Position within the parent's surface-local space.
            wl!(wl_proxy_marshal(subsurface, 1 /* set_position */, pos.x as i32, pos.y as i32));

            let ctx = egui::Context::default();
            ctx.set_fonts(crate::app_font_definitions());
            let renderer =
                egui_wgpu::Renderer::new(&rs.device, format, egui_wgpu::RendererOptions::default());

            Some(Self {
                parent,
                registry,
                compositor: globals.compositor,
                surface,
                subsurface,
                region_iface,
                ctx,
                main_ctx,
                renderer,
                device: rs.device.clone(),
                queue: rs.queue.clone(),
                wgpu_surface,
                cfg,
                pos,
                size,
                active: false,
                dirty_save: false,
                drag: None,
                resize: None,
                hover: None,
                swallow_release: false,
                tex_map: std::collections::HashMap::new(),
                pending_center: false,
            })
        }
    }

    /// Mini rect in parent-surface points.
    fn rect(&self) -> Rect {
        Rect::from_min_size(self.pos, self.size)
    }

    /// The parent's input region tracks the mini's rect so clicks
    /// outside it fall through the (ghosted) main window to whatever
    /// is underneath. Restoring `None` hands the whole surface back.
    pub fn set_parent_region(&self, on: bool) {
        {
            if !on {
                wl!(wl_proxy_marshal(
                    self.parent, 5 /* set_input_region */,
                    std::ptr::null_mut::<wl_proxy>(),
                ));
                return;
            }
            let region = wl!(wl_proxy_marshal_constructor(
                self.compositor, 1 /* create_region */,
                self.region_iface,
                std::ptr::null::<c_void>(),
            ));
            if region.is_null() {
                return;
            }
            let r = self.rect();
            wl!(wl_proxy_marshal(
                region,
                1, // wl_region.add
                r.min.x as i32,
                r.min.y as i32,
                r.width() as i32,
                r.height() as i32,
            ));
            wl!(wl_proxy_marshal(self.parent, 5 /* set_input_region */, region));
            wl!(wl_proxy_destroy(region));
        }
    }

    fn set_position(&self) {
        {
            wl!(wl_proxy_marshal(
                self.subsurface,
                1 /* set_position */,
                self.pos.x as i32,
                self.pos.y as i32,
            ));
        }
    }

    /// Apply a new size: reconfigure the swapchain at BUF_SCALE.
    fn resize_surface(&mut self) {
        self.size = snap_size(self.size);
        let (w, h) = (
            self.size.x as u32 * BUF_SCALE as u32,
            self.size.y as u32 * BUF_SCALE as u32,
        );
        if self.cfg.width != w || self.cfg.height != h {
            self.cfg.width = w;
            self.cfg.height = h;
            self.wgpu_surface.configure(&self.device, &self.cfg);
        }
        self.set_position();
        self.set_parent_region(true);
    }

    /// Center the mini on the screen — the "no saved position" case.
    /// The ghost parent fills the workarea while mini mode is live,
    /// so parent-local coordinates are screen coordinates.
    pub fn centered_pos(&self, size: Vec2) -> Pos2 {
        let screen = self
            .main_ctx
            .input(|i| {
                i.viewport()
                    .monitor_size
                    .map(|s| Rect::from_min_size(Pos2::ZERO, s))
                    .or(i.viewport().inner_rect)
            })
            .unwrap_or_else(|| Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)));
        screen.center() - size / 2.0
    }

    /// Flip applied at the veil's full-black point, replacing
    /// `apply_window_mode` when the subsurface path is live. The
    /// parent never resizes — entering mini mode it maximizes into
    /// an invisible full-workarea ghost (so the floating surface can
    /// park anywhere and still receive input through the parent's
    /// region); leaving, it restores itself. That is also how the
    /// normal window gets its position back "for free": it never
    /// actually moved.
    pub fn apply_mode(&mut self, settings: &mut crate::settings::Settings) {
        let w = &mut settings.window;
        if w.mini {
            // Leaving mini — remember its rect, restore the parent.
            w.mini_x = Some(self.pos.x);
            w.mini_y = Some(self.pos.y);
            w.mini_width = self.size.x;
            w.mini_height = self.size.y;
            w.mini = false;
            self.hide();
            self.main_ctx
                .send_viewport_cmd(egui::ViewportCommand::Maximized(false));
        } else {
            self.activate(settings);
        }
        crate::settings::save(settings);
    }

    /// Map the subsurface — used both on the normal→mini flip and on
    /// a launch straight into mini mode. Restores the saved rect or
    /// centers when none exists.
    pub fn activate(&mut self, settings: &mut crate::settings::Settings) {
        let w = &mut settings.window;
        w.mini = true;
        let size = Vec2::new(
            w.mini_width.max(crate::mini::MIN_SIZE.x),
            w.mini_height.max(crate::mini::MIN_SIZE.y),
        );
        self.size = snap_size(size);
        self.pos = match w.mini_x.zip(w.mini_y) {
            Some((x, y)) => Pos2::new(x, y),
            None => {
                // No monitor size yet → re-center on a later frame
                // once the viewport info has caught up.
                self.pending_center = self
                    .main_ctx
                    .input(|i| i.viewport().monitor_size.is_none());
                self.centered_pos(size)
            }
        };
        // Ghost first: the parent stretches to the workarea so
        // mini-local offsets can cover the whole screen.
        self.main_ctx
            .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        self.active = true;
        self.resize_surface();
        self.set_parent_region(true);
    }

    fn hide(&mut self) {
        {
            wl!(wl_proxy_marshal(
                self.surface,
                1, /* attach */ std::ptr::null_mut::<wl_proxy>(),
                0,
                0,
            ));
            wl!(wl_proxy_marshal(self.surface, 6 /* commit */));
        }
        self.set_parent_region(false);
        self.active = false;
        self.drag = None;
        self.resize = None;
    }

    /// Watch the parent's input stream for the presses that move or
    /// resize the floating surface. Coordinates arrive in parent
    /// points; geometry updates go straight to the compositor.
    /// `s` is the UI scale (1/scale_factor) the shell was laid out
    /// with — used to mirror the chrome's title/button geometry.
    /// Returns true while a drag/resize owns the pointer.
    fn route(&mut self, raw: &egui::RawInput, s: f32) -> bool {
        use egui::{Event, PointerButton};
        let rect = self.rect();
        let title_h = crate::MINI_TITLE_BAR_HEIGHT * s;
        // Right-side button strip (Close, Minimize, gear, switch:
        // 4 x 35*s wide, 25*s spacing, 20*s right pad) and the
        // left back-button corner keep their clicks — presses there
        // must not start a move.
        let strip_w = (20.0 + 4.0 * 35.0 + 3.0 * 25.0) * s;
        let back_sq = crate::MINI_CHROME_HEIGHT * s;
        let title = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.min.y + title_h));
        let draggable = |p: Pos2| {
            title.contains(p)
                && p.x < rect.max.x - strip_w
                && !(p.x < rect.min.x + back_sq && p.y < rect.min.y + back_sq)
        };
        for ev in &raw.events {
            match ev {
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    ..
                } => {
                    self.hover = Some(*pos);
                    if let Some(dir) = self.grip_dir(*pos) {
                        self.resize = Some((dir, *pos, self.pos, self.size));
                    } else if draggable(*pos) {
                        self.drag = Some(*pos);
                    }
                }
                Event::Touch { pos, .. } => self.hover = Some(*pos),
                Event::PointerMoved(pos) => {
                    self.hover = Some(*pos);
                    if let Some(last) = self.drag {
                        self.pos += *pos - last;
                        self.drag = Some(*pos);
                        self.set_position();
                        self.set_parent_region(true);
                    } else if let Some((dir, start, p0, s0)) = self.resize {
                        let d = *pos - start;
                        let (mut p, mut s) = (p0, s0);
                        match dir {
                            ResizeDirection::East => s.x = (s0.x + d.x).max(crate::mini::MIN_SIZE.x),
                            ResizeDirection::South => {
                                s.y = (s0.y + d.y).max(crate::mini::MIN_SIZE.y)
                            }
                            ResizeDirection::SouthEast => {
                                s = (s0 + d).max(crate::mini::MIN_SIZE);
                            }
                            ResizeDirection::West => {
                                let dx = d.x.min(s0.x - crate::mini::MIN_SIZE.x);
                                p.x = p0.x + dx;
                                s.x = s0.x - dx;
                            }
                            ResizeDirection::North => {
                                let dy = d.y.min(s0.y - crate::mini::MIN_SIZE.y);
                                p.y = p0.y + dy;
                                s.y = s0.y - dy;
                            }
                            ResizeDirection::NorthWest => {
                                let dx = d.x.min(s0.x - crate::mini::MIN_SIZE.x);
                                let dy = d.y.min(s0.y - crate::mini::MIN_SIZE.y);
                                p = Pos2::new(p0.x + dx, p0.y + dy);
                                s = Vec2::new(s0.x - dx, s0.y - dy);
                            }
                            ResizeDirection::NorthEast => {
                                let dy = d.y.min(s0.y - crate::mini::MIN_SIZE.y);
                                p.y = p0.y + dy;
                                s = Vec2::new(
                                    (s0.x + d.x).max(crate::mini::MIN_SIZE.x),
                                    s0.y - dy,
                                );
                            }
                            ResizeDirection::SouthWest => {
                                let dx = d.x.min(s0.x - crate::mini::MIN_SIZE.x);
                                p.x = p0.x + dx;
                                s = Vec2::new(
                                    s0.x - dx,
                                    (s0.y + d.y).max(crate::mini::MIN_SIZE.y),
                                );
                            }
                        }
                        self.pos = p;
                        self.size = s;
                        self.resize_surface();
                        self.resize = Some((dir, start, p0, s0));
                    }
                }
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                } => {
                    self.hover = Some(*pos);
                    if self.drag.is_some() || self.resize.is_some() {
                        self.drag = None;
                        self.resize = None;
                        self.dirty_save = true;
                        // This release must not reach the widget the
                        // pointer was parked on during the gesture.
                        self.swallow_release = true;
                    }
                }
                Event::PointerGone => self.hover = None,
                _ => {}
            }
        }
        self.drag.is_some() || self.resize.is_some()
    }

    /// Which resize grip (if any) owns a parent-space point: the
    /// outer GRIP band of the mini rect.
    fn grip_dir(&self, pos: Pos2) -> Option<ResizeDirection> {
        let r = self.rect();
        if !r.contains(pos) {
            return None;
        }
        let l = pos.x < r.min.x + GRIP;
        let rt = pos.x > r.max.x - GRIP;
        let t = pos.y < r.min.y + GRIP;
        let b = pos.y > r.max.y - GRIP;
        use ResizeDirection::*;
        match (t, b, l, rt) {
            (true, false, true, false) => Some(NorthWest),
            (true, false, false, true) => Some(NorthEast),
            (false, true, true, false) => Some(SouthWest),
            (false, true, false, true) => Some(SouthEast),
            (true, false, false, false) => Some(North),
            (false, true, false, false) => Some(South),
            (false, false, true, false) => Some(West),
            (false, false, false, true) => Some(East),
            _ => None,
        }
    }

    /// Build this frame's RawInput for the private context: same
    /// stream as the parent, pointer coordinates translated into
    /// mini-local space, viewport info describing the subsurface.
    fn mini_input(&mut self, raw: &egui::RawInput, interactive: bool) -> egui::RawInput {
        let mut out = raw.clone();
        let rect = Rect::from_min_size(Pos2::ZERO, self.size);
        out.screen_rect = Some(rect);
        let mut info = egui::ViewportInfo::default();
        info.inner_rect = Some(rect);
        // No outer_rect — the geometry mirror must not learn this
        // surface's (meaningless) origin; position is owned here.
        info.outer_rect = None;
        info.monitor_size = raw.viewport().monitor_size;
        info.native_pixels_per_point = Some(BUF_SCALE as f32);
        info.minimized = Some(false);
        info.maximized = Some(false);
        info.fullscreen = Some(false);
        info.occluded = Some(false);
        info.focused = Some(interactive);
        out.viewports = [(egui::ViewportId::ROOT, info)].into_iter().collect();
        let off = self.pos.to_vec2();
        // A button release that ended our own drag/resize must not
        // click whatever widget the pointer was parked on — moving
        // the surface kept its local position on the press target.
        let swallow = std::mem::take(&mut self.swallow_release);
        for ev in &mut out.events {
            match ev {
                egui::Event::PointerMoved(p) => *p -= off,
                egui::Event::Touch { pos, .. } => *pos -= off,
                egui::Event::PointerButton { pos, pressed, .. } => {
                    *pos = if swallow && !*pressed {
                        Pos2::new(-1.0, -1.0)
                    } else {
                        *pos - off
                    };
                }
                _ => {}
            }
        }
        out
    }

    /// Forward the outputs egui produced for the floating window to
    /// the real one — cursor shape, clipboard, IME rect (shifted by
    /// our offset), and the commands that must reach the compositor.
    fn forward_output(&self, full: &egui::FullOutput) {
        let po = &full.platform_output;
        // Grip-band and in-flight gesture cursors win over whatever
        // widget the pointer happens to hover.
        let cursor = if let Some((dir, ..)) = self.resize {
            cursor_icon(dir)
        } else if self.drag.is_some() {
            egui::CursorIcon::Grabbing
        } else if let Some(dir) = self.hover.and_then(|p| self.grip_dir(p)) {
            cursor_icon(dir)
        } else {
            po.cursor_icon
        };
        self.main_ctx.set_cursor_icon(cursor);
        if let Some(ime) = &po.ime {
            let mut ime = ime.clone();
            ime.rect = ime.rect.translate(self.pos.to_vec2());
            self.main_ctx.output_mut(|o| o.ime = Some(ime));
        }
        if !po.commands.is_empty() {
            self.main_ctx
                .output_mut(|o| o.commands.extend(po.commands.iter().cloned()));
        }
        if let Some(vout) = full.viewport_output.get(&egui::ViewportId::ROOT) {
            for cmd in &vout.commands {
                match cmd {
                    // The close affordance in the mini chrome quits
                    // the app; minimize hides the (ghosted) parent.
                    egui::ViewportCommand::Close
                    | egui::ViewportCommand::Minimized(_) => {
                        self.main_ctx.send_viewport_cmd(cmd.clone());
                    }
                    _ => {}
                }
            }
        }
    }

    /// Upload this pass's texture deltas to the gpu. MUST run on
    /// every pass — including inactive ones — or the renderer's
    /// textures drift out of sync with the manager (a dropped atlas
    /// resize leaves a stale small texture that the next patch
    /// overruns).
    pub fn sync_textures(&mut self, full: &egui::FullOutput) {
        for (id, deltas) in &full.textures_delta.set {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        for id in &full.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }

    /// Tessellate + present one frame of the private context.
    pub fn present(&mut self, full: &egui::FullOutput) {
        let mut clipped = self
            .ctx
            .tessellate(full.shapes.clone(), full.pixels_per_point);
        // Shared textures carry main-context ids — remap to this
        // context's ids (see tex_map) before they hit the renderer.
        for c in &mut clipped {
            if let egui::epaint::Primitive::Mesh(m) = &mut c.primitive {
                if let Some(&mapped) = self.tex_map.get(&m.texture_id) {
                    m.texture_id = mapped;
                }
            }
        }
        let frame = match self.wgpu_surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost => {
                self.wgpu_surface.configure(&self.device, &self.cfg);
                return;
            }
            // Timeout/Occluded/Validation — try again next frame.
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let sd = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.cfg.width, self.cfg.height],
            pixels_per_point: BUF_SCALE as f32,
        };
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("miniwin"),
            });
        let mut bufs = self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut enc,
            &clipped,
            &sd,
        );
        {
            let mut rpass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("miniwin"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &clipped, &sd);
        }
        bufs.push(enc.finish());
        self.queue.submit(bufs);
        self.queue.present(frame);
    }

    /// One frame of the floating window: route drags/resizes at the
    /// parent level, run the private egui pass, forward its output,
    /// present to the subsurface.
    ///
    /// `ui_body` is the app's whole frame body — it runs inside the
    /// mini context exactly as it runs in the main one.
    pub fn frame(
        &mut self,
        raw: &egui::RawInput,
        s: f32,
        ui_body: impl FnOnce(&mut egui::Ui, Option<&mut MiniWin>),
    ) {
        if self.pending_center {
            let known = self
                .main_ctx
                .input(|i| i.viewport().monitor_size.is_some());
            if known {
                self.pos = self.centered_pos(self.size);
                self.pending_center = false;
                self.set_position();
                self.set_parent_region(true);
            }
        }
        self.route(raw, s);
        let input = self.mini_input(raw, true);
        let mctx = self.ctx.clone();
        let mut body = Some(ui_body);
        let mut full = mctx.run_pass(input, |mc| {
            mc.root_ui(|ui| {
                if let Some(b) = body.take() {
                    b(ui, Some(self));
                }
            });
        });
        // The run itself may have flipped the mode flag again
        // (mini→normal toggle): hide after the pass, not mid-paint.
        self.forward_output(&full);
        self.sync_textures(&full);
        self.sync_pending();
        if self.active {
            self.present(&full);
        } else {
            self.hide();
        }
        full.textures_delta.clear();
    }

    /// Drain queued shared-texture deltas and this context's own
    /// pending delta to the gpu, and free mini copies of shared
    /// textures the main side dropped. Runs on every frame — even
    /// while hidden — so neither queue grows unboundedly during a
    /// long normal-mode stretch.
    pub fn sync_pending(&mut self) {
        // Shared covers/icons get a fresh id in THIS context's
        // manager; the alloc queues a full delta that the drain
        // below uploads.
        for (main_id, delta) in crate::shared_tex::take_pending() {
            self.tex_map.entry(main_id).or_insert_with(|| {
                self.ctx.tex_manager().write().alloc(
                    format!("shared-{main_id:?}"),
                    delta.image,
                    delta.options,
                )
            });
        }
        let mut tex_map = std::mem::take(&mut self.tex_map);
        tex_map.retain(|main_id, mini_id| {
            if crate::shared_tex::is_allocated(main_id) {
                true
            } else {
                self.ctx.tex_manager().write().free(*mini_id);
                false
            }
        });
        self.tex_map = tex_map;
        let mut d = self.ctx.tex_manager().write().take_delta();
        for (id, deltas) in &d.set {
            for delta in deltas {
                // Managed(0) is allocated with a 0x0 placeholder at
                // Context::default() — a pass's end_pass normally
                // overwrites that delta with the real atlas before
                // upload, but an inactive ctx never passes, so the
                // placeholder reaches the gpu raw and wgpu rejects a
                // zero-dimension texture.
                if delta.image.width() == 0 || delta.image.height() == 0 {
                    continue;
                }
                self.renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        for id in &d.free {
            self.renderer.free_texture(id);
        }
        // Dropping a non-empty TexturesDelta debug-panics.
        d.clear();
    }

    /// Geometry changed via a drag/resize — caller persists it.
    pub fn take_dirty_save(&mut self) -> bool {
        std::mem::take(&mut self.dirty_save)
    }
}

impl Drop for MiniWin {
    fn drop(&mut self) {
        self.set_parent_region(false);
        wl!(wl_proxy_destroy(self.subsurface));
        wl!(wl_proxy_destroy(self.surface));
        wl!(wl_proxy_destroy(self.registry));
    }
}

/// Cursor shape for a resize grip direction.
fn cursor_icon(dir: ResizeDirection) -> egui::CursorIcon {
    use egui::CursorIcon::*;
    match dir {
        ResizeDirection::North | ResizeDirection::South => ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => ResizeHorizontal,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => ResizeNeSw,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => ResizeNwSe,
    }
}


