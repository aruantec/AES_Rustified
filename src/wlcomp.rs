//! Nested Wayland compositor for emulator capture on Linux — the
//! Lacrima `LinuxCaptureHost` equivalent. The emulator is spawned as
//! a client of an in-process compositor: every frame it commits arrives
//! directly as a wl_buffer (shm or dmabuf), so there is no compositor
//! round-trip, no screen-recording permission dialog, and no window on
//! the user's desktop. Input goes the other way — we ARE the seat, so
//! pointer/keyboard events are injected at protocol level.
//!
//! Frame path (v1): client buffer -> GlesRenderer import -> render into
//! an offscreen renderbuffer -> ExportMem readback -> `FrameSlot` CPU
//! pixels, which the UI already knows how to paint. Frame callbacks are
//! answered immediately after consumption so the client runs uncapped.

use std::{
    ffi::OsString,
    os::unix::{fs::MetadataExt, io::OwnedFd},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::{
            dmabuf::{Dmabuf, DmabufFlags},
            Fourcc, Modifier,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        input::{Axis, ButtonState, KeyState},
        renderer::{
            element::{
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Element, Kind,
            },
            gles::{GlesRenderbuffer, GlesRenderer},
            utils::{draw_render_elements, on_commit_buffer_handler},
            Bind, Color32F, ExportMem, Frame, Offscreen, Renderer,
        },
    },
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_drm_syncobj, delegate_fractional_scale, delegate_idle_inhibit,
    delegate_keyboard_shortcuts_inhibit, delegate_kde_decoration, delegate_output,
    delegate_pointer_constraints, delegate_presentation, delegate_relative_pointer, delegate_seat,
    delegate_shm, delegate_single_pixel_buffer, delegate_viewporter, delegate_xdg_activation,
    delegate_xdg_shell, delegate_xwayland_shell,
    desktop::utils::send_frames_surface_tree,
    input::{
        keyboard::{FilterResult, KeyboardHandle, XkbConfig},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle, RelativeMotionEvent},
        Seat, SeatHandler, SeatState,
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            channel::{channel, Event as ChannelEvent, Sender as ChannelSender},
            generic::Generic,
            timer::{TimeoutAction, Timer},
            EventLoop, Interest, LoopHandle, LoopSignal, Mode as SourceMode, PostAction,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason, ObjectId},
            protocol::{wl_buffer, wl_output, wl_seat::WlSeat, wl_surface::WlSurface},
            Client, Display, DisplayHandle, Dispatch, GlobalDispatch, ListeningSocket, Resource,
        },
    },
    utils::{Buffer, Logical, Physical, Point, Rectangle, Serial, Size, Transform, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{
            DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
        },
        fractional_scale::{with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState},
        idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
        },
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsHandler,
        relative_pointer::RelativePointerManagerState,
        selection::{
            data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler},
            SelectionHandler,
        },
        shell::{
            kde::decoration::{KdeDecorationHandler, KdeDecorationState},
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        viewporter::ViewporterState,
        xdg_activation::{XdgActivationToken, XdgActivationTokenData, XdgActivationHandler, XdgActivationState},
        drm_syncobj::DrmSyncobjState,
        presentation::PresentationState,
        pointer_constraints::PointerConstraintsState,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
    },
};

use crate::{wgc, xenia::session_log as log};

/// Generated bindings for the vendored legacy `wl_drm` protocol
/// (`protocols/wayland-drm.xml`). XWayland's glamor uses this global to
/// discover the GBM render device and mint prime buffers; without it
/// XWayland falls back to software GL and never registers DRI3, which
/// kills Vulkan presentation for X11 emulators entirely.
mod wl_drm_proto {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_backend;
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/wayland-drm.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocols/wayland-drm.xml");
}
use wl_drm_proto::wl_drm::WlDrm;

/// Input events forwarded from the UI into the nested seat. Coordinates
/// are in the published frame's space (0,0 = the crop's top-left; the
/// compositor adds the scene origin back to get global coords).
pub enum WlInput {
    /// Absolute pointer position inside the captured surface.
    PointerMotion { x: f64, y: f64 },
    /// evdev button code (BTN_LEFT = 0x110, ...).
    PointerButton { button: u32, pressed: bool },
    /// Smooth scroll delta.
    PointerAxis { horizontal: f64, vertical: f64 },
    /// evdev key code (KEY_* constants; the caller translates egui keys).
    Key { code: u32, pressed: bool },
}

/// Handle to the running nested compositor. Dropping it stops the
/// event loop and disconnects every emulator client.
pub struct WlComp {
    socket_name: OsString,
    input_tx: ChannelSender<WlInput>,
    stop: Arc<AtomicBool>,
    loop_signal: LoopSignal,
    thread: Option<JoinHandle<()>>,
    /// XWayland's display number once its Ready event fires (0 = not
    /// yet / unavailable). Published by the compositor thread.
    x11_display: Arc<AtomicU32>,
}

impl WlComp {
    /// Start the compositor on its own thread. `frames` is the slot the
    /// capture publishes into; `got_client`/`got_frame` flip when the
    /// emulator connects / the first frame lands (session status
    /// tracking). `size` is the virtual output resolution the client
    /// renders at.
    pub fn start(
        size: (i32, i32),
        frames: Arc<Mutex<wgc::FrameSlot>>,
        got_client: Arc<AtomicBool>,
        got_frame: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let x11_display = Arc::new(AtomicU32::new(0));
        let (input_tx, input_rx) = channel::<WlInput>();
        // Handshake: the thread reports the bound socket name back so
        // the caller can set WAYLAND_DISPLAY before spawning.
        let (name_tx, name_rx) = std::sync::mpsc::channel();
        let stop2 = stop.clone();
        let x11_display2 = x11_display.clone();
        let thread = std::thread::Builder::new()
            .name("wl-comp".into())
            .spawn(move || {
                if let Err(e) = run(
                    size,
                    frames,
                    got_client,
                    got_frame,
                    input_rx,
                    stop2,
                    x11_display2,
                    name_tx,
                ) {
                    log(&format!("wlcomp error: {e}"));
                }
            })
            .map_err(|e| format!("spawn wlcomp thread: {e}"))?;
        let (socket_name, loop_signal) = name_rx
            .recv()
            .map_err(|_| "wlcomp died before binding".to_string())?
            .map_err(|e: String| e)?;
        Ok(Self {
            socket_name,
            input_tx,
            stop,
            loop_signal,
            thread: Some(thread),
            x11_display,
        })
    }

    /// The Wayland socket name (e.g. `wayland-3`) — set as
    /// `WAYLAND_DISPLAY` when spawning the emulator.
    pub fn socket_name(&self) -> &OsString {
        &self.socket_name
    }

    /// Forward one input event into the nested seat.
    pub fn send_input(&self, ev: WlInput) {
        if self.input_tx.send(ev).is_ok() {
            self.loop_signal.wakeup();
        }
    }

    /// Wait for XWayland to come up (it starts async on the compositor
    /// thread) and return its display number — e.g. `2` for `:2`.
    /// `None` when XWayland isn't available within `timeout`.
    pub fn wait_display(&self, timeout: Duration) -> Option<u32> {
        let deadline = Instant::now() + timeout;
        loop {
            let d = self.x11_display.load(Ordering::Relaxed);
            if d != 0 {
                return Some(d);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for WlComp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.loop_signal.wakeup();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Per-client state the wayland server attaches to each connection.
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Rank a DRM render node for GPU preference — dedicated VRAM wins,
/// which picks the discrete card over an APU's small stolen-memory
/// carveout. NVIDIA's proprietary driver doesn't expose
/// `mem_info_vram_total`, so its vendor id counts as discrete.
fn render_node_rank(device_dir: &std::path::Path) -> u64 {
    let vendor = std::fs::read_to_string(device_dir.join("vendor"))
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    let vram = std::fs::read_to_string(device_dir.join("mem_info_vram_total"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    vram + if vendor == 0x10de { 1u64 << 62 } else { 0 }
}

/// `vendor:device` of the strongest render node — for
/// `MESA_VK_DEVICE_SELECT`, which reorders RADV's physical-device
/// list. Without it Vulkan clients on iGPU+dGPU desktops (Ryzen APU +
/// big Radeon) enumerate the 2-CU iGPU first and `gpu=any`-style
/// pickers land on it. `None` when there's nothing to disambiguate.
pub(crate) fn vulkan_device_select() -> Option<String> {
    let mut nodes: Vec<(u64, u32, u32)> = Vec::new();
    for entry in std::fs::read_dir("/sys/class/drm").ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("renderD") {
            continue;
        }
        let dir = entry.path().join("device");
        let read_hex = |f: &str| {
            std::fs::read_to_string(dir.join(f))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        };
        let (Some(vendor), Some(device)) = (read_hex("vendor"), read_hex("device")) else {
            continue;
        };
        nodes.push((render_node_rank(&dir), vendor, device));
    }
    if nodes.len() < 2 {
        return None;
    }
    nodes.sort_by_key(|&(rank, vendor, device)| (rank, vendor, device));
    let (_, vendor, device) = *nodes.last().unwrap();
    Some(format!("{vendor:04x}:{device:04x}"))
}

/// The compositor. All state lives here; the calloop callbacks borrow
/// it mutably so nothing needs locking. Several `*State` fields are
/// never touched by us directly — they only need to exist for the
/// globals they registered at `new()` time to keep working.
#[allow(dead_code)]
struct Comp {
    display_handle: DisplayHandle,
    socket_name: OsString,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Comp>,
    output_state: OutputManagerState,
    data_device_state: DataDeviceState,
    dmabuf_state: DmabufState,
    fractional_scale: FractionalScaleManagerState,
    viewporter: ViewporterState,
    single_pixel_buffer: SinglePixelBufferState,
    presentation: PresentationState,
    idle_inhibit: IdleInhibitManagerState,
    pointer_constraints: PointerConstraintsState,
    relative_pointer: RelativePointerManagerState,
    keyboard_shortcuts_inhibit: KeyboardShortcutsInhibitState,
    xdg_activation: XdgActivationState,
    cursor_shape: CursorShapeManagerState,
    kde_decoration: KdeDecorationState,
    drm_syncobj: Option<DrmSyncobjState>,
    xwayland_shell: XWaylandShellState,
    /// The running X11 window manager + mapped X11 windows, when
    /// XWayland is up. `x11_display` publishes the display number to
    /// the spawning thread (0 = not ready yet).
    x11_wm: Option<X11Wm>,
    x11_surfaces: Vec<X11Surface>,
    x11_display: Arc<AtomicU32>,

    seat: Seat<Comp>,
    keyboard: KeyboardHandle<Comp>,
    pointer: PointerHandle<Comp>,
    /// Last pointer position the UI tunnel sent — drawn into the
    /// published frame so the user has a visible cursor reference.
    /// Frame-space (the published crop), not compositor space.
    pointer_loc: Option<Point<f64, Logical>>,
    /// Origin of the last published scene crop. Input arrives in
    /// frame space (the cropped readback), so protocol dispatch adds
    /// this back to get compositor-global coordinates.
    scene_loc: Point<f64, Logical>,
    /// Where each toplevel was last rendered — the surface's
    /// global-space origin, needed for pointer focus so clients get
    /// correct surface-local coordinates.
    toplevel_locs: std::collections::HashMap<ObjectId, Point<f64, Logical>>,
    output: Output,
    start: Instant,
    /// Render+readback on every commit is a synchronous GPU
    /// round-trip — a client committing faster than the capture rate
    /// would serialize the whole pipeline. Publishes are coalesced to
    /// ~120 Hz; a skipped commit arms a short timer to flush the
    /// pending frame.
    last_publish: Instant,
    pending_publish: bool,
    publish_timer_set: bool,
    loop_handle: Option<LoopHandle<'static, Comp>>,

    /// Every mapped toplevel, oldest first — the emulator's main
    /// window is the first; dialogs (Qt messages, CSD menus) stack on
    /// top and all render into the same frame. Input goes to the
    /// newest one, which is also what the user sees on top.
    toplevels: Vec<ToplevelSurface>,
    /// GLES renderer for dmabuf import + readback, together with the
    /// render node it lives on (needed for linux-drm-syncobj). None
    /// when no usable EGL device exists — shm clients still work.
    renderer: Option<(GlesRenderer, PathBuf)>,
    /// Readback target, reallocated when the frame size changes.
    target: Option<GlesRenderbuffer>,
    target_size: Size<i32, Buffer>,

    frames: Arc<Mutex<wgc::FrameSlot>>,
    got_client: Arc<AtomicBool>,
    got_frame: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,

    /// Diagnostics — commit/publish counters so a stuck pipeline is
    /// visible in boot.log without tracing.
    commits: u64,
    publishes: u64,
    empty_publishes: u64,
}

fn run(
    size: (i32, i32),
    frames: Arc<Mutex<wgc::FrameSlot>>,
    got_client: Arc<AtomicBool>,
    got_frame: Arc<AtomicBool>,
    input_rx: smithay::reexports::calloop::channel::Channel<WlInput>,
    stop: Arc<AtomicBool>,
    x11_display: Arc<AtomicU32>,
    name_tx: std::sync::mpsc::Sender<Result<(OsString, LoopSignal), String>>,
) -> Result<(), String> {
    let mut display: Display<Comp> = Display::new().map_err(|e| format!("display: {e}"))?;
    let dh = display.handle();

    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "aes-capture");
    let keyboard = seat
        .add_keyboard(XkbConfig::default(), 200, 25)
        .map_err(|e| format!("keyboard: {e}"))?;
    let pointer = seat.add_pointer();

    // One virtual output = the virtual display the emulator believes
    // it's fullscreen on. Its mode is the resolution we capture at.
    let mode = Mode {
        size: Size::from(size),
        refresh: 144_000,
    };
    let output = Output::new(
        "AES-CAPTURE".into(),
        PhysicalProperties {
            size: Size::from((0, 0)),
            subpixel: Subpixel::Unknown,
            make: "AES".into(),
            model: "Capture".into(),
        },
    );
    output.create_global::<Comp>(&dh);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some(Point::from((0, 0))),
    );
    output.set_preferred(mode);

    // Headless GLES context on a render node — needed to import client
    // dmabufs (Vulkan/GL emulators never hand us shm buffers). Devices
    // are tried strongest-first: on iGPU+dGPU desktops enumeration
    // order lands on the APU's render node, which makes the
    // compositor's render+readback (and XWayland's glamor, via wl_drm)
    // share the weakest GPU with the game.
    let renderer = (|| -> Option<(GlesRenderer, PathBuf)> {
        let mut devices: Vec<EGLDevice> = EGLDevice::enumerate().ok()?.collect();
        devices.sort_by_key(|d| {
            d.render_device_path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_os_string()))
                .map(|n| render_node_rank(&Path::new("/sys/class/drm").join(n).join("device")))
                .unwrap_or(0)
        });
        for device in devices.into_iter().rev() {
            let Ok(path) = device.render_device_path() else {
                continue;
            };
            if let Ok(display) = unsafe { EGLDisplay::new(device.clone()) } {
                if let Ok(context) = EGLContext::new(&display) {
                    if let Ok(r) = unsafe { GlesRenderer::new(context) } {
                        log(&format!("wlcomp renderer on {}", path.display()));
                        return Some((r, path));
                    }
                }
            }
        }
        None
    })();
    if renderer.is_none() {
        log("wlcomp: no EGL device — dmabuf capture disabled");
    }

    // Advertise every pixel format the GLES renderer can sample so
    // clients pick a supported dmabuf layout — with default feedback
    // naming the render node as main_device. Without the device
    // tranche Mesa's wayland EGL can't resolve a GPU ("failed to get
    // driver name for fd -1") and clients fall back to llvmpipe —
    // xemu's GL display path then dies on a create_display_image
    // assertion.
    let mut dmabuf_state = DmabufState::new();
    if let Some((r, path)) = renderer.as_ref() {
        let formats = r.egl_context().display().dmabuf_texture_formats().clone();
        let feedback = std::fs::metadata(path).ok().and_then(|m| {
            DmabufFeedbackBuilder::new(m.rdev(), formats.iter().cloned())
                .build()
                .ok()
        });
        match feedback {
            Some(f) => {
                let _global: DmabufGlobal = dmabuf_state
                    .create_global_with_default_feedback::<Comp>(&dh, &f);
            }
            None => {
                let _global: DmabufGlobal =
                    dmabuf_state.create_global::<Comp>(&dh, formats.iter().cloned());
            }
        }
    }

    // linux-drm-syncobj: explicit-sync fence support so Vulkan clients
    // can use it when available.
    let drm_syncobj = renderer
        .as_ref()
        .and_then(|(_, path)| std::fs::File::open(path).ok())
        .map(|file| {
            let fd: OwnedFd = file.into();
            let dev_fd = smithay::utils::DeviceFd::from(fd);
            let drm_fd = smithay::backend::drm::DrmDeviceFd::new(dev_fd);
            DrmSyncobjState::new::<Comp>(&dh, drm_fd)
        });

    // wl_drm — legacy GBM wayland interface. XWayland's glamor uses it
    // to discover the render device and mint prime buffers for
    // presentation; buffers are registered as real smithay dmabufs so
    // they import through the normal GLES path. (EGL_WL_bind_
    // wayland_display is not exposed on render-node displays, so we
    // can't let Mesa publish this global for us.)
    let drm_device = renderer
        .as_ref()
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"));
    dh.create_global::<Comp, WlDrm, PathBuf>(2, drm_device);

    let mut state = Comp {
        display_handle: dh.clone(),
        socket_name: OsString::new(),
        compositor_state: CompositorState::new::<Comp>(&dh),
        xdg_shell_state: XdgShellState::new::<Comp>(&dh),
        shm_state: ShmState::new::<Comp>(&dh, vec![]),
        seat_state,
        output_state: OutputManagerState::new_with_xdg_output::<Comp>(&dh),
        data_device_state: DataDeviceState::new::<Comp>(&dh),
        dmabuf_state,
        fractional_scale: FractionalScaleManagerState::new::<Comp>(&dh),
        viewporter: ViewporterState::new::<Comp>(&dh),
        single_pixel_buffer: SinglePixelBufferState::new::<Comp>(&dh),
        presentation: PresentationState::new::<Comp>(&dh, libc::CLOCK_MONOTONIC as u32),
        idle_inhibit: IdleInhibitManagerState::new::<Comp>(&dh),
        pointer_constraints: PointerConstraintsState::new::<Comp>(&dh),
        relative_pointer: RelativePointerManagerState::new::<Comp>(&dh),
        keyboard_shortcuts_inhibit: KeyboardShortcutsInhibitState::new::<Comp>(&dh),
        xdg_activation: XdgActivationState::new::<Comp>(&dh),
        cursor_shape: CursorShapeManagerState::new::<Comp>(&dh),
        kde_decoration: KdeDecorationState::new::<Comp>(
            &dh,
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode::Server,
        ),
        drm_syncobj,
        xwayland_shell: XWaylandShellState::new::<Comp>(&dh),
        x11_wm: None,
        x11_surfaces: Vec::new(),
        x11_display,
        seat,
        keyboard,
        pointer,
        pointer_loc: None,
        scene_loc: Point::from((0.0, 0.0)),
        toplevel_locs: std::collections::HashMap::new(),
        output,
        start: Instant::now(),
        last_publish: Instant::now(),
        pending_publish: false,
        publish_timer_set: false,
        loop_handle: None,
        toplevels: Vec::new(),
        renderer,
        target: None,
        target_size: Size::from((0, 0)),
        frames,
        got_client,
        got_frame,
        stop,
        commits: 0,
        publishes: 0,
        empty_publishes: 0,
    };

    // Socket inside XDG_RUNTIME_DIR — clients resolve it from
    // WAYLAND_DISPLAY relative to that dir.
    let socket = ListeningSocket::bind_auto("wayland", 0..32)
        .map_err(|e| format!("bind wayland socket: {e}"))?;
    state.socket_name = socket.socket_name().unwrap().to_os_string();
    log(&format!("wlcomp socket {:?}", state.socket_name));

    let mut event_loop = EventLoop::<Comp>::try_new().map_err(|e| format!("calloop: {e}"))?;
    let handle = event_loop.handle();
    state.loop_handle = Some(handle.clone());

    // XWayland for X11-only emulators (Xenia's Linux build has no
    // wayland Vulkan surface — it presents through xcb). XWayland
    // itself is a wayland client of ours; its Ready event carries the
    // X11 connection the window manager runs on.
    match XWayland::spawn(
        &dh,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        std::process::Stdio::null(),
        std::process::Stdio::inherit(),
        |_| {},
    ) {
        Ok((xwayland, x_client)) => {
            let wm_handle = handle.clone();
            if let Err(e) = handle.insert_source(xwayland, move |event, _, state| {
                match event {
                    XWaylandEvent::Ready {
                        x11_socket,
                        display_number,
                    } => {
                        log(&format!("wlcomp xwayland ready :{display_number}"));
                        match X11Wm::start_wm(wm_handle.clone(), x11_socket, x_client.clone()) {
                            Ok(wm) => {
                                state.x11_wm = Some(wm);
                                state.x11_display.store(display_number, Ordering::Relaxed);
                            }
                            Err(e) => log(&format!("wlcomp xwm start: {e}")),
                        }
                    }
                    XWaylandEvent::Error => log("wlcomp xwayland startup error"),
                }
            }) {
                log(&format!("wlcomp xwayland source: {e}"));
            }
        }
        Err(e) => log(&format!("wlcomp xwayland spawn failed (X11 clients unavailable): {e}")),
    }

    // New client connections.
    handle
        .insert_source(
            Generic::new(socket, Interest::READ, SourceMode::Level),
            |_, socket, state| {
                loop {
                    match socket.accept() {
                        Ok(Some(stream)) => {
                            state.got_client.store(true, Ordering::Relaxed);
                            log("wlcomp client connected");
                            let cs = Arc::new(ClientState {
                                compositor_state: CompositorClientState::default(),
                            });
                            if let Err(e) = state.display_handle.insert_client(stream, cs) {
                                log(&format!("wlcomp insert_client: {e}"));
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log(&format!("wlcomp accept: {e}"));
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("socket source: {e}"))?;

    // Client request dispatch — the display's poll fd is readable
    // whenever a client sent requests. The fd is dup'd so the Display
    // can move into the dispatch closure.
    let poll_fd = display
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup display fd: {e}"))?;
    let mut display = display;
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, SourceMode::Level),
            move |_, _, state| {
                let _ = display.dispatch_clients(state);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("display source: {e}"))?;

    // Input events from the UI thread.
    handle
        .insert_source(input_rx, move |event, _, state| {
            if let ChannelEvent::Msg(ev) = event {
                state.handle_input(ev);
            }
        })
        .map_err(|e| format!("input source: {e}"))?;

    // Report the socket name back — the spawn env needs it.
    let _ = name_tx.send(Ok((state.socket_name.clone(), event_loop.get_signal())));

    while !state.stop.load(Ordering::Relaxed) {
        let _ = event_loop.dispatch(Some(Duration::from_millis(500)), &mut state);
        let _ = state.display_handle.flush_clients();
    }
    Ok(())
}

impl Comp {
    /// The surface currently considered the captured window — the
    /// topmost mapped surface, X11 (XWayland) or Wayland. Last mapped
    /// wins; dialogs take input focus over the main window, matching
    /// their stacking.
    fn active_surface(&self) -> Option<WlSurface> {
        self.x11_surfaces
            .iter()
            .rev()
            .find_map(|w| w.wl_surface())
            .or_else(|| self.toplevels.last().map(|t| t.wl_surface().clone()))
    }

    /// Where `surface` lives in compositor-global space — its origin
    /// is the second half of the pointer focus tuple (clients get
    /// `location - origin` as their surface-local coords). X11
    /// windows sit at their configured geometry; toplevels at the
    /// position publish() last rendered them.
    fn surface_origin(&self, surface: &WlSurface) -> Point<f64, Logical> {
        for w in self.x11_surfaces.iter().rev() {
            if w.wl_surface().as_ref() == Some(surface) {
                return w.geometry().loc.to_f64();
            }
        }
        self.toplevel_locs
            .get(&surface.id())
            .copied()
            .unwrap_or_default()
    }

    /// Milliseconds since compositor start — protocol timestamp base.
    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Route one UI-originated event into the nested seat.
    fn handle_input(&mut self, ev: WlInput) {
        // Clone the seat handles out — smithay methods take `&mut self`
        // (the compositor state) alongside the handle, which would
        // otherwise conflict with borrowing the field.
        let pointer = self.pointer.clone();
        let keyboard = self.keyboard.clone();
        match ev {
            WlInput::PointerMotion { x, y } => {
                // (x, y) is in frame space — the published crop of the
                // scene. The cursor overlay draws in that same space;
                // protocol coords translate back to compositor space.
                let frame_pos = Point::<f64, Logical>::from((x, y));
                let delta = self
                    .pointer_loc
                    .map(|p| Point::from((frame_pos.x - p.x, frame_pos.y - p.y)))
                    .unwrap_or_default();
                self.pointer_loc = Some(frame_pos);
                let loc = Point::<f64, Logical>::from((
                    frame_pos.x + self.scene_loc.x,
                    frame_pos.y + self.scene_loc.y,
                ));
                // Focus's second element is the surface's global
                // origin, NOT the pointer position — smithay derives
                // surface-local coords as `location - origin`.
                let focus = self
                    .active_surface()
                    .map(|s| (s.clone(), self.surface_origin(&s)));
                pointer.motion(
                    self,
                    focus.clone(),
                    &MotionEvent {
                        location: loc,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: self.time(),
                    },
                );
                // Keep relative-pointer clients (pointer-locked games)
                // fed as well.
                pointer.relative_motion(
                    self,
                    focus,
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        utime: self.time() as u64 * 1000,
                    },
                );
                // wl_pointer v5 groups pointer events into frames —
                // XWayland defers dispatch until the frame arrives.
                pointer.frame(self);
            }
            WlInput::PointerButton { button, pressed } => {
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: self.time(),
                    },
                );
                pointer.frame(self);
            }
            WlInput::PointerAxis {
                horizontal,
                vertical,
            } => {
                let mut frame =
                    AxisFrame::new(self.time()).source(smithay::backend::input::AxisSource::Wheel);
                if horizontal != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal);
                }
                if vertical != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical);
                }
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            WlInput::Key { code, pressed } => {
                let state = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                // `code` is evdev; smithay's Keycode is the X scheme
                // (evdev + 8) — winit does the same shift.
                keyboard.input::<(), _>(
                    self,
                    (code + 8).into(),
                    state,
                    SERIAL_COUNTER.next_serial(),
                    self.time(),
                    |_, _, _| FilterResult::Forward,
                );
            }
        }
    }

    /// Render every mapped toplevel (oldest first = bottom of the
    /// stack) and publish the composited pixels into the shared frame
    /// slot. Called after every client commit — the emulator's main
    /// window and any Qt dialogs all contribute.
    fn publish(&mut self) {
        if self.toplevels.is_empty() && self.x11_surfaces.is_empty() {
            return;
        }
        let Some((renderer, _)) = self.renderer.as_mut() else {
            return;
        };
        let output_size: Size<i32, Physical> = self
            .output
            .current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| Size::from((1920, 1080)));

        // Import + render each surface tree. Element textures are
        // cached in the surfaces' RendererSurfaceState, so unchanged
        // regions skip re-import. Non-fullscreen toplevels (dialogs)
        // are centered over the output instead of hugging 0,0.
        self.publishes += 1;
        self.last_publish = Instant::now();
        self.pending_publish = false;
        self.toplevel_locs.clear();
        let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        for toplevel in &self.toplevels {
            let surface = toplevel.wl_surface();
            let first: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                renderer,
                surface,
                (0, 0),
                1.0,
                1.0,
                Kind::Unspecified,
            );
            if first.is_empty() {
                continue;
            }
            let bounds = first
                .iter()
                .map(|e| e.geometry(1.0.into()))
                .fold(Rectangle::<i32, Physical>::default(), |acc, r| {
                    acc.merge(r)
                });
            let loc = if bounds.size.w < output_size.w || bounds.size.h < output_size.h {
                (
                    (output_size.w - bounds.size.w).max(0) / 2,
                    (output_size.h - bounds.size.h).max(0) / 2,
                )
            } else {
                (0, 0)
            };
            self.toplevel_locs.insert(
                surface.id(),
                Point::<f64, Logical>::from((loc.0 as f64, loc.1 as f64)),
            );
            elements.extend(render_elements_from_surface_tree(
                renderer,
                surface,
                loc,
                1.0,
                1.0,
                Kind::Unspecified,
            ));
        }
        // XWayland windows — same pipeline, their wl_surfaces arrive
        // via the XWayland client's association. Rendered at their
        // configured geometry (fullscreen unless a dialog asked for
        // smaller).
        for window in &self.x11_surfaces {
            if let Some(surface) = window.wl_surface() {
                let loc = window.geometry().loc.to_physical(1);
                elements.extend(render_elements_from_surface_tree(
                    renderer,
                    &surface,
                    loc,
                    1.0,
                    1.0,
                    Kind::Unspecified,
                ));
            }
        }
        if elements.is_empty() {
            self.empty_publishes += 1;
            if self.empty_publishes <= 3 || self.empty_publishes % 300 == 0 {
                log(&format!(
                    "wlcomp publish: no renderable elements (#{})",
                    self.empty_publishes
                ));
            }
            return;
        }
        // Elements can sit at an offset inside the scene (a centered
        // non-fullscreen window, a dialog stacked over the main one) —
        // render the full extent, then crop the readback to the merged
        // bounds so the published frame is exactly the content with no
        // dead margin shifting the picture inside the view.
        let scene = elements
            .iter()
            .map(|e| e.geometry(1.0.into()))
            .fold(Rectangle::<i32, Physical>::default(), |acc, r| {
                acc.merge(r)
            });
        if scene.size.w <= 0 || scene.size.h <= 0 {
            return;
        }
        self.scene_loc = Point::<f64, Logical>::from((scene.loc.x as f64, scene.loc.y as f64));
        let extent = scene.loc + scene.size;
        let frame_size: Size<i32, Physical> = Size::from((extent.x, extent.y));
        let buf_size = frame_size.to_logical(1).to_buffer(1, Transform::Normal);

        // Recreate the readback target on size change.
        if self.target_size != buf_size {
            match renderer.create_buffer(Fourcc::Abgr8888, buf_size) {
                Ok(rb) => {
                    self.target = Some(rb);
                    self.target_size = buf_size;
                }
                Err(e) => {
                    log(&format!("wlcomp renderbuffer: {e}"));
                    return;
                }
            }
        }
        let Some(target) = self.target.as_mut() else {
            return;
        };

        let mut framebuffer = match renderer.bind(target) {
            Ok(fb) => fb,
            Err(e) => {
                log(&format!("wlcomp bind: {e}"));
                return;
            }
        };
        let Ok(mut frame) =
            renderer.render(&mut framebuffer, frame_size, Transform::Normal)
        else {
            return;
        };
        let damage_phys = Rectangle::<i32, Physical>::new((0, 0).into(), frame_size);
        let _ = frame.clear(Color32F::TRANSPARENT, &[damage_phys]);
        let _ = draw_render_elements(&mut frame, 1.0, &elements, &[damage_phys]);
        let _ = frame.finish();

        // Scale is 1 throughout (physical == logical == buffer).
        let copy_rect = Rectangle::<i32, Buffer>::new(
            Point::from((scene.loc.x, scene.loc.y)),
            Size::from((scene.size.w, scene.size.h)),
        );
        let Ok(mapping) = renderer.copy_framebuffer(&framebuffer, copy_rect, Fourcc::Abgr8888) else {
            return;
        };
        let Ok(pixels) = renderer.map_texture(&mapping) else {
            return;
        };

        let mut slot = self.frames.lock().unwrap_or_else(|p| p.into_inner());
        slot.w = scene.size.w as usize;
        slot.h = scene.size.h as usize;
        slot.rgba.clear();
        slot.rgba.extend_from_slice(pixels);
        // Software cursor — the OS cursor is hidden over the capture,
        // so the seat's pointer position gets painted into the frame
        // (white core + dark ring: visible on light and dark content).
        if let Some(loc) = self.pointer_loc {
            let (cx, cy) = (loc.x.round() as i64, loc.y.round() as i64);
            let (w, h) = (slot.w as i64, slot.h as i64);
            for dy in -8..=8i64 {
                for dx in -8..=8i64 {
                    let d2 = dx * dx + dy * dy;
                    let (px, py) = (cx + dx, cy + dy);
                    if d2 > 64 || px < 0 || py < 0 || px >= w || py >= h {
                        continue;
                    }
                    let i = ((py * w + px) * 4) as usize;
                    let (v, a) = if d2 <= 25 { (255u8, 255u8) } else { (0, 255) };
                    slot.rgba[i] = v;
                    slot.rgba[i + 1] = v;
                    slot.rgba[i + 2] = v;
                    slot.rgba[i + 3] = a;
                }
            }
        }
        slot.seq += 1;
        // Diagnostics — dump two late published frames so capture-side
        // geometry can be verified against what the UI paints.
        if matches!(self.publishes, 300 | 1500) {
            let _ = image::save_buffer(
                format!("/tmp/wlcomp-frame-{}.png", self.publishes),
                &slot.rgba,
                slot.w as u32,
                slot.h as u32,
                image::ExtendedColorType::Rgba8,
            );
        }
        drop(slot);
        let first = !self.got_frame.swap(true, Ordering::Relaxed);
        if first {
            let nonzero = self
                .frames
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .rgba
                .iter()
                .any(|&b| b != 0);
            log(&format!(
                "wlcomp first frame {:?} scene={:?} elems={} nonzero={nonzero}",
                scene.size,
                scene.loc,
                elements.len()
            ));
        }

        // Answer frame callbacks immediately — client pacing follows
        // its own render speed instead of a phantom vsync. Covers both
        // wayland toplevels and X11 windows via XWayland — a client
        // that throttles on wl_surface.frame stalls forever without
        // this.
        let surfaces: Vec<WlSurface> = self
            .toplevels
            .iter()
            .map(|t| t.wl_surface().clone())
            .chain(self.x11_surfaces.iter().filter_map(|w| w.wl_surface()))
            .collect();
        for surface in &surfaces {
            send_frames_surface_tree(
                surface,
                &self.output,
                self.start.elapsed(),
                Some(Duration::ZERO),
                |_, _| None,
            );
        }
    }
}

// -- smithay protocol handlers ---------------------------------------

impl CompositorHandler for Comp {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland connects as a wayland client too — its ClientData
        // is XWaylandClientData, not ours.
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<smithay::xwayland::XWaylandClientData>()
            .expect("client state")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.commits += 1;
        if self.commits <= 5 || self.commits % 600 == 0 {
            log(&format!(
                "wlcomp commit #{} top={} x11={} pub={} empty={}",
                self.commits,
                self.toplevels.len(),
                self.x11_surfaces.len(),
                self.publishes,
                self.empty_publishes
            ));
        }
        // Republish on commits — but render+readback is a synchronous
        // GPU round-trip, so coalesce to ~120 Hz. Clients committing
        // faster than that (XWayland damage bursts, unlocked framerate)
        // would otherwise stall the shared GPU pipe on every commit.
        // Skipped commits arm a one-shot timer so the latest state is
        // always published promptly even if the client goes quiet.
        const MIN_PUBLISH: Duration = Duration::from_millis(8);
        if self.last_publish.elapsed() >= MIN_PUBLISH {
            self.publish();
        } else {
            self.pending_publish = true;
            if !self.publish_timer_set {
                if let Some(handle) = self.loop_handle.clone() {
                    self.publish_timer_set = true;
                    let delay = MIN_PUBLISH - self.last_publish.elapsed();
                    if handle
                        .insert_source(Timer::from_duration(delay), |_, _, state| {
                            state.publish_timer_set = false;
                            if std::mem::take(&mut state.pending_publish) {
                                state.publish();
                            }
                            TimeoutAction::Drop
                        })
                        .is_err()
                    {
                        self.publish_timer_set = false;
                    }
                }
            }
        }
    }
}

impl BufferHandler for Comp {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl XdgShellHandler for Comp {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Fullscreen on the virtual output — the client renders at the
        // capture resolution and draws no decorations.
        if let Some(mode) = self.output.current_mode() {
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.set(xdg_toplevel::State::Activated);
                state.size = Some(mode.size.to_logical(1));
            });
        }
        surface.send_configure();
        self.toplevels.push(surface.clone());
        log("wlcomp toplevel mapped");
        // Deliver keyboard focus immediately — the emulator only gets
        // input once a surface claims focus.
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(
            self,
            Some(surface.wl_surface().clone()),
            SERIAL_COUNTER.next_serial(),
        );
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        if let Some(mode) = self.output.current_mode() {
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(mode.size.to_logical(1));
            });
        }
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(mode) = self.output.current_mode() {
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.size = Some(mode.size.to_logical(1));
            });
        }
        surface.send_configure();
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(mode) = self.output.current_mode() {
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Maximized);
                state.size = Some(mode.size.to_logical(1));
            });
        }
        surface.send_configure();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        _positioner: PositionerState,
        token: u32,
    ) {
        surface.send_repositioned(token);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.toplevels.retain(|t| t != &surface);
        log(&format!("wlcomp toplevel destroyed ({} left)", self.toplevels.len()));
        // Re-focus whatever is now on top so keyboard input follows.
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(
            self,
            self.active_surface(),
            SERIAL_COUNTER.next_serial(),
        );
    }
}

impl ShmHandler for Comp {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Comp {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Comp> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Comp>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Comp>, _image: CursorImageStatus) {}
}

impl smithay::wayland::output::OutputHandler for Comp {}

impl DataDeviceHandler for Comp {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl SelectionHandler for Comp {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for Comp {}
impl ServerDndGrabHandler for Comp {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Comp>) {}
}

impl DmabufHandler for Comp {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Comp>();
    }
}

impl FractionalScaleHandler for Comp {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        smithay::wayland::compositor::with_states(&surface, |states| {
            with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(1.0);
            });
        });
    }
}

impl PointerConstraintsHandler for Comp {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}

impl smithay::wayland::tablet_manager::TabletSeatHandler for Comp {}

impl KeyboardShortcutsInhibitHandler for Comp {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit
    }

    fn new_inhibitor(&mut self, _inhibitor: KeyboardShortcutsInhibitor) {}
}

impl XdgActivationHandler for Comp {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
    }
}

impl IdleInhibitHandler for Comp {
    fn inhibit(&mut self, _surface: WlSurface) {}

    fn uninhibit(&mut self, _surface: WlSurface) {}
}

impl KdeDecorationHandler for Comp {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration
    }
}

// wl_drm — legacy GBM wayland interface. XWayland's glamor binds it
// to discover the render device (global data is the device path) and
// mints presentation buffers via create_prime_buffer, which we turn
// into real smithay dmabufs so they import like zwp_linux_dmabuf_v1
// buffers.
impl GlobalDispatch<WlDrm, PathBuf> for Comp {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: smithay::reexports::wayland_server::New<WlDrm>,
        device_path: &PathBuf,
        data_init: &mut smithay::reexports::wayland_server::DataInit<'_, Self>,
    ) {
        let drm = data_init.init(resource, ());
        log(&format!(
            "wlcomp wl_drm bound, device {}",
            device_path.display()
        ));
        drm.device(device_path.to_string_lossy().into_owned());
        for f in [Fourcc::Xrgb8888 as u32, Fourcc::Argb8888 as u32] {
            drm.format(f);
        }
        drm.capabilities(1); // WL_DRM_CAPABILITY_PRIME
    }
}

impl Dispatch<WlDrm, ()> for Comp {
    fn request(
        _state: &mut Self,
        _client: &Client,
        drm: &WlDrm,
        request: wl_drm_proto::wl_drm::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut smithay::reexports::wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_drm_proto::wl_drm::Request::Authenticate { .. } => {
                // Render nodes need no drmAuth magic — always succeed.
                drm.authenticated();
            }
            wl_drm_proto::wl_drm::Request::CreateBuffer { id, .. }
            | wl_drm_proto::wl_drm::Request::CreatePlanarBuffer { id, .. } => {
                // gbm-name-based buffers are the pre-prime legacy path;
                // XWayland never uses them (it requires CAPABILITY_PRIME).
                log("wlcomp wl_drm legacy create_buffer (unsupported)");
                data_init.init(id, ());
            }
            wl_drm_proto::wl_drm::Request::CreatePrimeBuffer {
                id,
                name: fd,
                width,
                height,
                format,
                offset0,
                stride0,
                offset1,
                stride1,
                offset2,
                stride2,
            } => {
                let buffer = Fourcc::try_from(format)
                    .ok()
                    .and_then(|fourcc| {
                        let mut builder = Dmabuf::builder(
                            (width.max(1), height.max(1)),
                            fourcc,
                            Modifier::Invalid,
                            DmabufFlags::empty(),
                        );
                        // wl_drm planes share the passed fd — dup it
                        // for the extra planes a multi-plane format
                        // would need.
                        let fd1 =
                            (stride1 > 0).then(|| fd.try_clone().ok()).flatten();
                        let fd2 =
                            (stride2 > 0).then(|| fd.try_clone().ok()).flatten();
                        builder.add_plane(
                            fd,
                            0,
                            offset0.max(0) as u32,
                            stride0.max(0) as u32,
                        );
                        if let Some(f) = fd1 {
                            builder.add_plane(
                                f,
                                1,
                                offset1.max(0) as u32,
                                stride1.max(0) as u32,
                            );
                        }
                        if let Some(f) = fd2 {
                            builder.add_plane(
                                f,
                                2,
                                offset2.max(0) as u32,
                                stride2.max(0) as u32,
                            );
                        }
                        builder.build()
                    });
                match buffer {
                    Some(dmabuf) => {
                        data_init.init(id, dmabuf);
                    }
                    None => {
                        log("wlcomp wl_drm create_prime_buffer: bad params");
                        data_init.init(id, ());
                    }
                }
            }
        }
    }
}

// Buffers minted by the legacy (non-prime) wl_drm requests carry `()`
// user data — separate from smithay's managed wl_buffer dispatch.
impl Dispatch<wl_buffer::WlBuffer, ()> for Comp {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut smithay::reexports::wayland_server::DataInit<'_, Self>,
    ) {
    }
}

impl XWaylandShellHandler for Comp {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, surface: X11Surface) {
        log(&format!(
            "wlcomp x11 surface associated '{}'",
            surface.title()
        ));
    }
}

impl XwmHandler for Comp {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.x11_wm.as_mut().expect("xwm event before start_wm")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        log(&format!("wlcomp x11 new_window '{}'", window.title()));
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        log(&format!("wlcomp x11 new_override_redirect '{}'", window.title()));
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        log(&format!("wlcomp x11 map request '{}'", window.title()));
        // Fullscreen on the virtual output — the emulator renders at
        // the capture resolution.
        let _ = window.configure(Rectangle::new(
            Point::from((0, 0)),
            Size::from((1920, 1080)),
        ));
        let _ = window.set_mapped(true);
    }

    fn map_window_notify(&mut self, _xwm: XwmId, window: X11Surface) {
        log("wlcomp x11 window mapped");
        self.x11_surfaces.push(window);
        if let Some(surface) = self.x11_surfaces.last().and_then(|w| w.wl_surface()) {
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_surfaces.push(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_surfaces.retain(|w| w != &window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_surfaces.retain(|w| w != &window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // We own the layout: managed windows stay inside the virtual
        // output. Clients that restore a remembered geometry (Flycast
        // maps at 2559x1380) get clamped to the output and resize to
        // the notified size.
        let out = self
            .output
            .current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| Size::from((1920, 1080)));
        let mut geo = window.geometry();
        geo.size = Size::from((
            w.map(|v| v as i32).unwrap_or(geo.size.w).clamp(1, out.w),
            h.map(|v| v as i32).unwrap_or(geo.size.h).clamp(1, out.h),
        ));
        geo.loc = Point::from((
            x.unwrap_or(geo.loc.x).clamp(0, (out.w - geo.size.w).max(0)),
            y.unwrap_or(geo.loc.y).clamp(0, (out.h - geo.size.h).max(0)),
        ));
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _b: u32, _e: ResizeEdge) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Set the state first — clients key off _NET_WM_STATE_FULLSCREEN
        // (Xenia hides its menu bar once fullscreen actually applies),
        // then snap the window to the output size.
        let _ = window.set_fullscreen(true);
        let _ = window.configure(Rectangle::new(
            Point::from((0, 0)),
            Size::from((1920, 1080)),
        ));
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_fullscreen(false);
    }

    fn property_notify(&mut self, _xwm: XwmId, _window: X11Surface, _p: WmWindowProperty) {}
}

impl smithay::wayland::drm_syncobj::DrmSyncobjHandler for Comp {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.drm_syncobj.as_mut()
    }
}

delegate_compositor!(Comp);
delegate_shm!(Comp);
delegate_xdg_shell!(Comp);
delegate_seat!(Comp);
delegate_output!(Comp);
delegate_data_device!(Comp);
delegate_dmabuf!(Comp);
delegate_viewporter!(Comp);
delegate_fractional_scale!(Comp);
delegate_single_pixel_buffer!(Comp);
delegate_presentation!(Comp);
delegate_idle_inhibit!(Comp);
delegate_pointer_constraints!(Comp);
delegate_relative_pointer!(Comp);
delegate_keyboard_shortcuts_inhibit!(Comp);
delegate_xdg_activation!(Comp);
delegate_cursor_shape!(Comp);
delegate_kde_decoration!(Comp);
delegate_drm_syncobj!(Comp);
delegate_xwayland_shell!(Comp);

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: host a real Wayland GL client and confirm frames
    /// reach the FrameSlot. Skips quietly when no client binary or
    /// render node exists (CI/containers).
    #[test]
    fn wayland_client_frames_arrive() {
        let client = [
            "/usr/bin/es2gears_wayland",
            "/usr/bin/eglgears_wayland",
            "/usr/bin/vkcube",
        ]
        .iter()
        .find(|p| std::path::Path::new(p).exists());
        let Some(client) = client else {
            return;
        };
        if !std::path::Path::new("/dev/dri").exists() {
            return;
        }

        let frames = Arc::new(Mutex::new(wgc::FrameSlot::default()));
        let got_client = Arc::new(AtomicBool::new(false));
        let got_frame = Arc::new(AtomicBool::new(false));
        let comp = WlComp::start(
            (1280, 720),
            frames.clone(),
            got_client.clone(),
            got_frame.clone(),
        )
        .expect("wlcomp start");

        let mut child = std::process::Command::new(client)
            .env("WAYLAND_DISPLAY", comp.socket_name())
            .env_remove("DISPLAY")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("client spawn");

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && !got_frame.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let (w, h, len, seq) = {
            let s = frames.lock().unwrap_or_else(|p| p.into_inner());
            (s.w, s.h, s.rgba.len(), s.seq)
        };
        let _ = child.kill();
        let _ = child.wait();

        assert!(got_client.load(Ordering::Relaxed), "client never connected");
        assert!(got_frame.load(Ordering::Relaxed), "no frame within 20s");
        assert!(w > 0 && h > 0, "frame has no size");
        assert_eq!(len, w * h * 4, "pixel buffer size mismatch");
        assert!(seq > 0);
    }

    /// Manual reproduction aid: hold a compositor socket open for
    /// HOLD_SECS (default 60) so a real emulator can be launched
    /// against it from a shell — `cargo test hold_socket --
    /// --nocapture`, then `WAYLAND_DISPLAY=<name> ./emulator`.
    #[test]
    #[ignore]
    fn hold_socket() {
        let secs: u64 = std::env::var("HOLD_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let frames = Arc::new(Mutex::new(wgc::FrameSlot::default()));
        let comp = WlComp::start(
            (1920, 1080),
            frames.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("wlcomp start");
        eprintln!("SOCKET={:?}", comp.socket_name());
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut last = 0u64;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            let s = frames.lock().unwrap_or_else(|p| p.into_inner());
            if s.seq != last {
                last = s.seq;
                eprintln!(
                    "frame seq={} {}x{} len={}",
                    s.seq, s.w, s.h, s.rgba.len()
                );
            }
        }
    }
}
