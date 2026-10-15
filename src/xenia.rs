//! The Xenia (Xbox 360) emulator handler — a port of Lacrima's
//! `XeniaHandler` + the handler-base window plumbing. Launches
//! `xenia.exe <rom>`, resolves the game's render window (Xenia draws
//! into a `temp_d3d_window` child rather than its main window),
//! strips decorations, parks it at the far corner of the virtual
//! desktop at 0% opacity — still visible to DWM so Windows Graphics
//! Capture keeps producing frames — and hands the HWND to `wgc`.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use std::os::windows::io::AsRawHandle;

use windows::core::BOOL;
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_ACCESS_RIGHTS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use eframe::egui;

use crate::wgc;

// Lacrima's PauseEmulator/ResumeEmulator — the ntdll suspend pair
// (OpenProcess + NtSuspendProcess / NtResumeProcess).
#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: *mut core::ffi::c_void) -> i32;
    fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
}

const SUSPEND_ACCESS: PROCESS_ACCESS_RIGHTS =
    PROCESS_ACCESS_RIGHTS(PROCESS_SUSPEND_RESUME.0 | PROCESS_QUERY_LIMITED_INFORMATION.0);

/// Owns a Windows Job Object with KILL_ON_JOB_CLOSE: when the last
/// handle to the job closes — on normal Drop AND on an abrupt process
/// death where destructors never run (panic=abort, crash, kill) — the
/// OS terminates every process assigned to it. This is what keeps a
/// launched emulator from being orphaned when the app dies.
struct JobGuard(HANDLE);

// A job handle is a process-global kernel handle — safe to move
// across threads.
unsafe impl Send for JobGuard {}

impl JobGuard {
    /// Job with kill-on-close containing `child`. `None` when the OS
    /// refuses (assignment can fail if the process is already in a
    /// non-nesting job) — the session still runs, it just loses the
    /// crash-safety net.
    fn new(child: &Child) -> Option<Self> {
        unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let ok = (|| {
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .ok()?;
                AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).ok()
            })()
            .is_some();
            if ok {
                Some(Self(job))
            } else {
                let _ = CloseHandle(job);
                None
            }
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Session lifecycle shared with the UI thread.
#[derive(Clone)]
pub enum Status {
    /// Searching the process tree for the render window.
    Resolving,
    /// Capture session attached — frames are flowing.
    Capturing,
    /// Setup failed (no window, WGC attach error, …).
    Failed(String),
    /// The emulator process exited or was stopped.
    Exited,
}

struct Shared {
    status: Status,
}

/// Which emulator a session wraps — controls the CLI args and the
/// window-resolution heuristics (Xenia's render surface is a
/// `temp_d3d_window` child; Flycast's is its main SDL/Qt window).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Xenia,
    Flycast,
}

/// A running Xenia launch: owns the child process, the
/// resolve/capture worker thread, and the WGC frame slot.
pub struct Session {
    child: Child,
    /// The launched game's path (used for the title/status line).
    pub game: PathBuf,
    status: Arc<Mutex<Shared>>,
    frames: Arc<Mutex<wgc::FrameSlot>>,
    /// The zero-copy shared-texture ring + cross-API fence for the
    /// DX12 interop display path (populated once WGC attaches).
    gpu: Arc<wgc::GpuState>,
    stop: Arc<AtomicBool>,
    /// Lacrima's IsEmulatorPaused — the process is suspended.
    paused: Arc<AtomicBool>,
    /// Live render adjustments the WGC swizzle reads each frame.
    render: Arc<wgc::RenderShared>,
    /// The captured render window — written by the worker once the
    /// capture target resolves; the Aspect-Ratio context menu resizes
    /// it (Lacrima's EmulatorTargetHwnd).
    hwnd: Arc<std::sync::atomic::AtomicUsize>,
    worker: Option<JoinHandle<()>>,
    /// Kill-on-close job containing the emulator — keeps it from
    /// being orphaned even by a hard crash. Declared last so it is
    /// the final field to drop.
    _job: Option<JobGuard>,
}

impl Session {
    /// Spawn the emulator (`kind`) with the game and start the
    /// resolve/capture worker. Lacrima inserts `--fullscreen` for
    /// fullscreen launches — we launch windowed instead since the
    /// window is hidden anyway and a windowed surface stays
    /// captureable when moved off-screen.
    /// `low_latency` shrinks the WGC frame pool to a single buffer.
    /// `profile_xuid` (Xenia only) is passed as
    /// `--logged_profile_slot_0_xuid` so a provisioned profile signs
    /// in without the (unclickable through capture) NoProfileDialog.
    pub fn start(
        exe: &str,
        game: &Path,
        low_latency: bool,
        profile_xuid: Option<u64>,
        kind: Kind,
    ) -> Result<Self, String> {
        let exe_path = PathBuf::from(exe);
        let work_dir = exe_path
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_default();
        let mut cmd = Command::new(exe);
        cmd.arg(game);
        if let Some(xuid) = profile_xuid {
            cmd.arg(format!("--logged_profile_slot_0_xuid={xuid:016X}"));
        }
        let child = cmd
            .current_dir(work_dir)
            .spawn()
            .map_err(|e| format!("failed to start emulator: {e}"))?;

        // Kill-on-close job FIRST — if anything below panics the
        // guard unwinds and the OS reaps the emulator for us.
        let job = JobGuard::new(&child);

        let pid = child.id();
        let status = Arc::new(Mutex::new(Shared {
            status: Status::Resolving,
        }));
        let frames = Arc::new(Mutex::new(wgc::FrameSlot::default()));
        let gpu = Arc::new(wgc::GpuState::default());
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let render = Arc::new(wgc::RenderShared::default());
        let hwnd = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = {
            let status = status.clone();
            let frames = frames.clone();
            let gpu = gpu.clone();
            let stop = stop.clone();
            let render = render.clone();
            let hwnd = hwnd.clone();
            std::thread::Builder::new()
                .name("xenia-session".into())
                .spawn(move || {
                    let status2 = status.clone();
                    // A panic in the resolve/capture plumbing must
                    // degrade to a failed session — never take the
                    // whole app down with the emulator.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        worker_main(pid, status, frames, gpu, stop, render, hwnd, low_latency, kind)
                    }))
                    .is_err()
                    {
                        status2
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .status = Status::Failed("capture worker crashed".into());
                    }
                })
                .expect("spawn xenia worker")
        };
        Ok(Self {
            child,
            game: game.to_path_buf(),
            status,
            frames,
            gpu,
            stop,
            paused,
            render,
            hwnd,
            worker: Some(worker),
            _job: job,
        })
    }

    /// Live render adjustments — write the atomics and the capture
    /// thread applies them on the very next frame.
    pub fn render(&self) -> &Arc<wgc::RenderShared> {
        &self.render
    }

    /// The captured render window — invalid until the worker resolves
    /// the capture target.
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd.load(Ordering::Relaxed) as *mut _)
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status
            .clone()
    }

    /// The latest captured frame slot (`seq` bumps per frame).
    pub fn frames(&self) -> &Arc<Mutex<wgc::FrameSlot>> {
        &self.frames
    }

    /// The GPU shared-frame ring for the zero-copy display path —
    /// `share` is populated once the capture's D3D11 side creates it.
    pub fn gpu_state(&self) -> &Arc<wgc::GpuState> {
        &self.gpu
    }

    /// Ask the worker to close the emulator gracefully (WM_CLOSE on
    /// the render window, TerminateProcess fallback) and tear down the
    /// capture. Status flips to `Exited` when done.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // A suspended emulator can't process WM_CLOSE — resume it so
        // the graceful path still applies (TerminateProcess catches
        // anything left).
        if self.paused.swap(false, Ordering::Relaxed) {
            set_process_suspended(self.child.id(), false);
        }
    }

    /// Lacrima's ToggleEmulatorPause — suspend/resume the emulator
    /// process (ntdll pair). True when the call succeeded.
    pub fn set_paused(&self, on: bool) -> bool {
        if set_process_suspended(self.child.id(), on) {
            self.paused.store(on, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// The windows tunneled input goes to. The capture target may be
    /// a bare surface window (`temp_d3d_window`) that ignores input,
    /// while the process's main window (where the ImGui/WndProc input
    /// listeners live) is its root or owner — so events go to the
    /// hwnd, its root ancestor AND its root-owner ancestor, deduped.
    /// Each WndProc ignores what isn't its own, so multiple targets
    /// are harmless.
    fn input_targets(&self) -> Vec<HWND> {
        let hwnd = self.hwnd();
        if hwnd.is_invalid() {
            return Vec::new();
        }
        let mut out = vec![hwnd];
        for mode in [GA_ROOT, GA_ROOTOWNER] {
            let anc = unsafe { GetAncestor(hwnd, mode) };
            if !anc.is_invalid() && !out.contains(&anc) {
                out.push(anc);
            }
        }
        out
    }

    /// Post a pointer message (`(x, y)` = client coords of the
    /// captured window) to every input target, each translated into
    /// its own client space. Returns how many windows got a post.
    pub fn post_at(&self, msg: u32, w: WPARAM, x: i32, y: i32) -> usize {
        let targets = self.input_targets();
        let n = targets.len();
        for t in targets {
            let mut pt = POINT { x, y };
            // Client→screen is relative to OUR capture window first,
            // then screen→client into the target's space.
            unsafe {
                let _ = ClientToScreen(self.hwnd(), &mut pt);
                let _ = ScreenToClient(t, &mut pt);
            }
            let lp = LPARAM((pt.x as isize & 0xFFFF) | ((pt.y as isize) << 16));
            let _ = unsafe { PostMessageW(Some(t), msg, w, lp) };
        }
        n
    }

    /// Key/char messages carry no coords — post the same lParam to
    /// every input target. Returns how many windows got a post.
    pub fn post(&self, msg: u32, w: WPARAM, l: LPARAM) -> usize {
        let targets = self.input_targets();
        let n = targets.len();
        for t in targets {
            let _ = unsafe { PostMessageW(Some(t), msg, w, l) };
        }
        n
    }

    /// The render window's client size in its own pixel space — the
    /// tunnel scales captured-frame coords by client/frame rather
    /// than assuming they're equal.
    pub fn client_size(&self) -> Option<(i32, i32)> {
        let hwnd = self.hwnd();
        if hwnd.is_invalid() {
            return None;
        }
        let mut r = RECT::default();
        unsafe { GetClientRect(hwnd, &mut r) }.ok()?;
        Some((r.right - r.left, r.bottom - r.top))
    }

    /// WM_MOUSEWHEEL takes *screen* coords in lParam — translate a
    /// client point for it.
    pub fn client_to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        let hwnd = self.hwnd();
        let mut p = POINT { x, y };
        unsafe {
            let _ = ClientToScreen(hwnd, &mut p);
        }
        (p.x, p.y)
    }

    /// True once the emulator process is gone (UI can drop the
    /// session — Drop reaps the child and detaches the worker).
    pub fn finished(&mut self) -> bool {
        matches!(self.status(), Status::Exited)
            || matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Detach — the worker finishes teardown on its own.
        self.worker.take();
    }
}

/// OpenProcess + NtSuspend/Resume — returns false when the process is
/// gone or the call fails (UI keeps the un-paused state then).
fn set_process_suspended(pid: u32, on: bool) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(SUSPEND_ACCESS, false, pid) else {
            return false;
        };
        let ok = if on {
            NtSuspendProcess(h.0) >= 0
        } else {
            NtResumeProcess(h.0) >= 0
        };
        let _ = CloseHandle(h);
        ok
    }
}

/// Resolve window → prepare/hide → attach capture → monitor. Runs on
/// its own thread so the UI never blocks on the ~12s resolve window.
fn worker_main(
    pid: u32,
    status: Arc<Mutex<Shared>>,
    frames: Arc<Mutex<wgc::FrameSlot>>,
    gpu: Arc<wgc::GpuState>,
    stop: Arc<AtomicBool>,
    render: Arc<wgc::RenderShared>,
    hwnd_out: Arc<std::sync::atomic::AtomicUsize>,
    low_latency: bool,
    kind: Kind,
) {
    let set_status = |s: Status| {
        status.lock().unwrap_or_else(|p| p.into_inner()).status = s;
    };

    let proc = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            pid,
        )
    }
    .unwrap_or_default();

    let alive = |proc: HANDLE| -> bool {
        if proc.is_invalid() {
            return true; // handle open failed — let Child::try_wait judge
        }
        (unsafe { WaitForSingleObject(proc, 0) }) == WAIT_TIMEOUT
    };

    // 1. Resolve the render window (Lacrima: up to 240 attempts at
    //    50ms — Xenia creates the d3d window lazily on game boot).
    let mut hwnd = HWND::default();
    for _ in 0..240 {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if kind == Kind::Xenia {
            dismiss_warning_dialogs(pid);
        }
        if !alive(proc) {
            break;
        }
        hwnd = resolve_capture_target(pid, kind);
        if !hwnd.is_invalid() {
            hwnd_out.store(hwnd.0 as usize, Ordering::Relaxed);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if hwnd.is_invalid() {
        set_status(if stop.load(Ordering::Relaxed) {
            Status::Exited
        } else if !alive(proc) {
            Status::Exited
        } else {
            Status::Failed("no render window found".into())
        });
        if !proc.is_invalid() {
            unsafe {
                let _ = TerminateProcess(proc, 0);
                let _ = CloseHandle(proc);
            }
        }
        return;
    }

    // 2. Prepare + hide. Lacrima: PrepareWindowForCaptureAttach on
    //    the target, PrepareProcessForCapture on every top-level
    //    window of the process (the render child lives inside a parent
    //    window — hiding only the child would leave Xenia's shell
    //    visible).
    let saved = prepare_window_for_capture(hwnd);
    hide_window(hwnd);
    let mut hidden_others = hide_process_windows(pid, hwnd);

    // 3. Attach WGC — frames land in the shared slots (CPU mirror +
    //    zero-copy GPU ring).
    let _capture = wgc::Capture::start(hwnd, frames.clone(), gpu, render, low_latency);
    set_status(Status::Capturing);

    // 4. Monitor: stop request, process exit, or capture failure.
    //    One delayed re-hide sweep catches windows Xenia creates
    //    after the target resolved (late console/dialog surfaces).
    let started = Instant::now();
    let mut rehidden = false;
    let mut capture_err: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) || !alive(proc) {
            break;
        }
        if !rehidden && started.elapsed() > Duration::from_millis(600) {
            rehidden = true;
            hidden_others.extend(hide_process_windows(pid, hwnd));
        }
        if let Some(err) = frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .err
            .clone()
        {
            capture_err = Some(err);
            break;
        }
        std::thread::sleep(Duration::from_millis(120));
    }

    drop(_capture);

    if let Some(err) = capture_err {
        // Capture died but the game may still run — put the windows
        // back so the user can interact with/close it.
        restore_window(hwnd, &saved);
        for (h, s) in hidden_others {
            restore_window(h, &s);
        }
        set_status(Status::Failed(err));
        if !proc.is_invalid() {
            unsafe {
                let _ = CloseHandle(proc);
            }
        }
        return;
    }

    // 5. Teardown — close Xenia: WM_CLOSE first, then force.
    if alive(proc) {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        let deadline = Instant::now() + Duration::from_millis(2500);
        while Instant::now() < deadline && alive(proc) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if alive(proc) && !proc.is_invalid() {
            unsafe {
                let _ = TerminateProcess(proc, 0);
            }
        }
    }
    if !proc.is_invalid() {
        unsafe {
            let _ = CloseHandle(proc);
        }
    }
    set_status(Status::Exited);
}

// ------------------------------------------------------------------
// Window discovery — a compact port of XeniaHandler's resolution:
// score render-surface child windows first (Xenia's temp_d3d_window),
// fall back to the best-looking top-level window.

/// Snapshot of state we strip so a failed capture can restore it.
struct SavedWindow {
    style: i64,
    exstyle: i64,
    rect: RECT,
    menu: Option<isize>,
}

struct EnumCtx {
    pid: u32,
    include_hidden: bool,
    out: Vec<HWND>,
}

unsafe extern "system" fn enum_top(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if !ctx.include_hidden && !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut wpid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut wpid));
    if wpid == ctx.pid {
        ctx.out.push(hwnd);
    }
    BOOL(1)
}

fn process_windows(pid: u32, include_hidden: bool) -> Vec<HWND> {
    let mut ctx = EnumCtx {
        pid,
        include_hidden,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_top),
            LPARAM(&mut ctx as *mut EnumCtx as isize),
        );
    }
    ctx.out
}

struct ChildCtx {
    pid: u32,
    best: HWND,
    best_score: i64,
}

unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut ChildCtx);
    let score = score_render_child(hwnd, ctx.pid);
    if score > ctx.best_score {
        ctx.best_score = score;
        ctx.best = hwnd;
    }
    BOOL(1)
}

/// Score a window as a Xenia render-surface child
/// (ScoreXeniaRenderChildCandidate).
fn score_render_child(hwnd: HWND, pid: u32) -> i64 {
    if hwnd.is_invalid() {
        return i64::MIN;
    }
    let mut wpid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut wpid));
    }
    if wpid != pid {
        return i64::MIN;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return i64::MIN;
    }
    let w = (r.right - r.left).max(0);
    let h = (r.bottom - r.top).max(0);
    if w < 320 || h < 180 {
        return i64::MIN;
    }
    let class = class_name(hwnd).to_lowercase();
    let title = window_title(hwnd);
    if is_xenia_message_box(hwnd, &title, &class) {
        return i64::MIN;
    }
    if class.contains("ime")
        || class.contains("observerwindow")
        || class.contains("titlebar")
        || class.contains("tooltip")
    {
        return i64::MIN;
    }
    let mut score = w as i64 * h as i64 * 10;
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        score += 500_000;
    }
    if class.starts_with("temp_d3d_window") {
        score += 50_000_000;
    }
    if class.contains("d3d") && class.contains("window") {
        score += 20_000_000;
    }
    if class.contains("render") || class.contains("swapchain") || class.contains("viewport") {
        score += 5_000_000;
    }
    if title.to_lowercase().contains("temp window") {
        score += 10_000_000;
    }
    score
}

/// IsLikelyFlyCastRenderWindow — the render window is a top-level
/// SDL/Qt surface; dialogs (settings/audio/video/…) are filtered by
/// title, and Flycast's own chrome gets excluded the same way
/// Lacrima does.
fn is_likely_flycast_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    const BAD: &[&str] = &[
        "settings",
        "audio",
        "video",
        "input",
        "controller",
        "cheat",
        "shader",
        "about",
    ];
    let title = window_title(hwnd);
    let t = title.trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let has_thick = (style & WS_THICKFRAME.0 as i64) == WS_THICKFRAME.0 as i64;
    let mut looks_primary = hwnd == main;
    let has_bad = BAD.iter().any(|w| t.contains(w));

    if !t.is_empty() {
        if t.contains("flycast") && !has_bad {
            looks_primary = false;
        }
        if has_bad {
            looks_primary = true;
        }
        if !looks_primary && t.chars().count() >= 2 {
            return true;
        }
    }
    if !class.is_empty()
        && (class.contains("sdl")
            || class.contains("glfw")
            || class.contains("qt")
            || class.contains("flycast"))
    {
        if !looks_primary {
            return true;
        }
        if !t.is_empty() && !has_bad {
            return true;
        }
        looks_primary |= has_caption && has_thick;
    }
    !looks_primary && (!has_caption || !t.is_empty())
}

/// IsLikelyXeniaRenderWindow — heuristic for top-level candidates
/// when no dedicated render child exists.
fn is_likely_render_window(hwnd: HWND, main: HWND, kind: Kind) -> bool {
    if kind == Kind::Flycast {
        return is_likely_flycast_window(hwnd, main);
    }
    if hwnd.is_invalid() {
        return false;
    }
    let title = window_title(hwnd);
    let class = class_name(hwnd).to_lowercase();
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let is_main = hwnd == main;
    if is_xenia_message_box(hwnd, &title, &class) {
        return false;
    }
    if !title.trim().is_empty() {
        let t = title.to_lowercase();
        for bad in [
            "console", "debug", "log", "settings", "controller", "tools", "about", "help",
            "warning", "error",
        ] {
            if t.contains(bad) {
                return false;
            }
        }
        if t.contains("xenia") && !t.contains("warning") {
            return true;
        }
    }
    if !class.is_empty() {
        if class.contains("sdl") || class.contains("xenia") {
            return true;
        }
        if class.starts_with("temp_d3d_window")
            || (class.contains("d3d") && class.contains("window"))
        {
            return true;
        }
    }
    !is_main && (!has_caption || !title.trim().is_empty())
}

/// ScoreProcessWindowCandidate (base) restricted to render windows.
fn score_top_level(hwnd: HWND, pid: u32, main: HWND, kind: Kind) -> i64 {
    if hwnd.is_invalid() {
        return i64::MIN;
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let mut wpid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut wpid));
    }
    if wpid != pid {
        return i64::MIN;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return i64::MIN;
    }
    let w = (r.right - r.left).max(0);
    let h = (r.bottom - r.top).max(0);
    if w <= 0 || h <= 0 {
        return i64::MIN;
    }
    // preferSpecificRenderWindow — non-render candidates are rejected.
    if !is_likely_render_window(hwnd, main, kind) {
        return i64::MIN;
    }
    // Flycast requires a real client area before assign
    // (IsStableCaptureCandidate — the UI settles at ≥640×360).
    if kind == Kind::Flycast && (w < 640 || h < 360) {
        return i64::MIN;
    }
    let mut score = w as i64 * h as i64 * 10;
    score += if visible { 100_000 } else { -100_000 };
    if unsafe { GetWindow(hwnd, GW_OWNER) }
        .map(|o| o.is_invalid())
        .unwrap_or(true)
    {
        score += 1_000_000;
    }
    if hwnd == main {
        score += 750_000;
    }
    if w >= 640 && h >= 360 {
        score += 250_000;
    }
    score += 5_000_000;
    score
}

/// One poll of ResolveCaptureTargetAsync: best render child, else
/// best render-like top-level window. `main` ≈ the biggest titled
/// visible top-level (Process.MainWindowHandle approximation).
fn resolve_capture_target(pid: u32, kind: Kind) -> HWND {
    let tops = process_windows(pid, true);
    if tops.is_empty() {
        return HWND::default();
    }
    let main = tops
        .iter()
        .copied()
        .filter(|&h| unsafe { IsWindowVisible(h) }.as_bool() && !window_title(h).is_empty())
        .max_by_key(|&h| {
            let mut r = RECT::default();
            let _ = unsafe { GetWindowRect(h, &mut r) };
            (r.right - r.left) as i64 * (r.bottom - r.top) as i64
        })
        .unwrap_or_default();

    // Render-surface child windows win for Xenia (its temp_d3d_window
    // lives under a top-level); Flycast renders in the top-level
    // window itself — a Qt child could only be chrome, so skip.
    if kind == Kind::Xenia {
        let mut ctx = ChildCtx {
            pid,
            best: HWND::default(),
            best_score: i64::MIN,
        };
        for &top in tops.iter().chain(std::iter::once(&main)) {
            if top.is_invalid() {
                continue;
            }
            unsafe {
                let _ = EnumChildWindows(
                    Some(top),
                    Some(enum_child),
                    LPARAM(&mut ctx as *mut ChildCtx as isize),
                );
            }
        }
        if ctx.best_score > i64::MIN && !ctx.best.is_invalid() {
            return ctx.best;
        }
    }

    tops.iter()
        .copied()
        .map(|h| (h, score_top_level(h, pid, main, kind)))
        .filter(|(_, s)| *s > i64::MIN)
        .max_by_key(|(_, s)| *s)
        .map(|(h, _)| h)
        .unwrap_or_default()
}

/// TryDismissXeniaWarningDialogs — click OK on "Xenia Warning/Error/
/// Help" dialogs (or WM_CLOSE them) so boot isn't blocked by a modal.
fn dismiss_warning_dialogs(pid: u32) {
    for hwnd in process_windows(pid, true) {
        let title = window_title(hwnd);
        let hit = title.eq_ignore_ascii_case("xenia help")
            || title.to_lowercase().contains("xenia warning")
            || title.to_lowercase().contains("xenia error");
        if !hit {
            continue;
        }
        if !click_dialog_button(hwnd, "OK") && !click_dialog_button(hwnd, "&OK") {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

struct ClickCtx {
    text: String,
    clicked: bool,
}

unsafe extern "system" fn enum_button(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut ClickCtx);
    if ctx.clicked {
        return BOOL(0);
    }
    if !class_name(hwnd).eq_ignore_ascii_case("button") {
        return BOOL(1);
    }
    if !window_title(hwnd).trim().eq_ignore_ascii_case(ctx.text.trim_start_matches('&')) {
        return BOOL(1);
    }
    SendMessageW(hwnd, BM_CLICK, Some(WPARAM(0)), Some(LPARAM(0)));
    ctx.clicked = true;
    BOOL(0)
}

fn click_dialog_button(dialog: HWND, text: &str) -> bool {
    let mut ctx = ClickCtx {
        text: text.to_string(),
        clicked: false,
    };
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog),
            Some(enum_button),
            LPARAM(&mut ctx as *mut ClickCtx as isize),
        );
    }
    ctx.clicked
}

/// IsXeniaMessageBoxWindow — dialog class or warning title or a small
/// warning/error-sized window.
fn is_xenia_message_box(hwnd: HWND, title: &str, class: &str) -> bool {
    let t = title.trim().to_lowercase();
    let c = class.trim().to_lowercase();
    if c == "#32770" {
        return true;
    }
    if t.contains("xenia warning") || t.contains("xenia error") || t == "xenia help" {
        return true;
    }
    if hwnd.is_invalid() {
        return false;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return false;
    }
    let w = (r.right - r.left).max(0);
    let h = (r.bottom - r.top).max(0);
    w > 0 && w < 640 && h > 0 && h < 360 && (t.contains("warning") || t.contains("error"))
}

// ------------------------------------------------------------------
// Prepare / hide / restore — Win32API.RemoveWindowDecorations +
// MoveAway + SetWindowOpacity(0). The window stays "visible" (never
// ShowWindow(SW_HIDE)) so the swapchain keeps presenting for WGC.

/// RemoveWindowDecorations: strip caption/sysmenu/thickframe/min/max,
/// drop the menu, and keep the *client* size (AdjustWindowRectEx) so
/// the render surface doesn't change aspect.
fn prepare_window_for_capture(hwnd: HWND) -> SavedWindow {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let exstyle = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as i64;
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    let menu = unsafe {
        let m = GetMenu(hwnd);
        if m.is_invalid() {
            None
        } else {
            let _ = SetMenu(hwnd, None);
            let _ = DrawMenuBar(hwnd);
            Some(m.0 as isize)
        }
    };
    let remove = (WS_CAPTION
        | WS_SYSMENU
        | WS_THICKFRAME
        | WS_MINIMIZEBOX
        | WS_MAXIMIZEBOX)
        .0 as i64;
    let new_style = style & !remove;
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style as isize);

        // Preserve client size — recompute the frame-less rect for the
        // current client area so the swapchain doesn't resize.
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client).is_ok() {
            let mut adj = client;
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if AdjustWindowRectEx(
                &mut adj,
                WINDOW_STYLE(new_style as u32),
                false,
                WINDOW_EX_STYLE(ex),
            )
            .is_ok()
            {
                let w = (adj.right - adj.left).max(0);
                let h = (adj.bottom - adj.top).max(0);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
                );
            }
        }
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
        );
    }
    SavedWindow {
        style,
        exstyle,
        rect,
        menu,
    }
}

/// MoveAway + SetWindowOpacity(0): park the window at the virtual
/// desktop's far corner (still on the desktop — SW_HIDE would kill
/// rendering) and make it fully transparent.
fn hide_window(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let dest_x = vx + (vw - 100).max(0);
        let dest_y = vy + (vh - 100).max(0);
        let _ = SetWindowPos(
            hwnd,
            None,
            dest_x,
            dest_y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        // WS_EX_TOOLWINDOW drops the taskbar/Alt-Tab button entirely
        // (the window stays DWM-composed, so WGC still gets frames);
        // WS_EX_APPWINDOW removal keeps it out even with a 0-alpha
        // layered window.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as i64;
        let new_ex = (ex | WS_EX_LAYERED.0 as i64 | WS_EX_TOOLWINDOW.0 as i64)
            & !(WS_EX_APPWINDOW.0 as i64);
        if new_ex != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex as isize);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
    }
}

/// Undo prepare+hide — used when capture attach fails so the user can
/// still see/close the emulator window.
fn restore_window(hwnd: HWND, saved: &SavedWindow) {
    if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as i64;
        if (ex & WS_EX_LAYERED.0 as i64) != 0 {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, saved.style as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, saved.exstyle as isize);
        if let Some(menu) = saved.menu {
            let _ = SetMenu(hwnd, Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                menu as *mut core::ffi::c_void,
            )));
            let _ = DrawMenuBar(hwnd);
        }
        let w = (saved.rect.right - saved.rect.left).max(0);
        let h = (saved.rect.bottom - saved.rect.top).max(0);
        let _ = SetWindowPos(
            hwnd,
            None,
            saved.rect.left,
            saved.rect.top,
            w,
            h,
            SWP_NOZORDER | SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
        );
    }
}

// ------------------------------------------------------------------
// Small Win32 text helpers.

/// PrepareProcessForCapture — hide every visible top-level window of
/// the process (strip decorations, park at the virtual-desktop
/// corner, opacity 0), except the capture target itself. Returns the
/// saved states for failure-path restore.
fn hide_process_windows(pid: u32, skip: HWND) -> Vec<(HWND, SavedWindow)> {
    let mut out = Vec::new();
    for hwnd in process_windows(pid, false) {
        if hwnd == skip || hwnd.is_invalid() {
            continue;
        }
        let saved = prepare_window_for_capture(hwnd);
        hide_window(hwnd);
        out.push((hwnd, saved));
    }
    out
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Job guards for emulator processes launched without a Session
/// (launch_setup). Kept for the process lifetime so the kill-on-close
/// job still fires if the app exits or crashes while a setup window
/// is open — the guards themselves only matter at process teardown.
static SETUP_JOBS: Mutex<Vec<JobGuard>> = Mutex::new(Vec::new());

/// Lacrima's ResizeWindowToAspectRatioInPlace — resize the captured
/// window's top-level ancestor to `aspect` at ~1920px wide, clamped
/// to the virtual screen (the render child follows its parent's
/// bounds). The window stays parked at its hidden spot — only its
/// size, and so the capture's native aspect, changes.
pub fn apply_aspect_ratio(hwnd: HWND, aspect: f64) {
    if hwnd.is_invalid() || aspect <= 0.0 {
        return;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let target = if root.is_invalid() { hwnd } else { root };
    let mut w = 1920i32;
    let mut h = (w as f64 / aspect).round().max(1.0) as i32;
    let (sw, sh) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if sw > 0 && w > sw {
        w = sw;
        h = (w as f64 / aspect).round().max(1.0) as i32;
    }
    if sh > 0 && h > sh {
        h = sh;
        w = (h as f64 * aspect).round().max(1.0) as i32;
    }
    unsafe {
        let _ = SetWindowPos(
            target,
            None,
            0,
            0,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
        );
    }
}

/// Lacrima's LaunchCurrentSectionHandlerSetup — spawn the emulator
/// executable with no game so its own settings UI opens. A plain
/// shell launch: not captured, not tracked — but still job-guarded so
/// a dying app can't leave an orphan emulator process.
pub fn launch_setup(exe: &str) -> Result<(), String> {
    let exe_path = PathBuf::from(exe);
    let child = Command::new(exe)
        .current_dir(
            exe_path
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_default(),
        )
        .spawn()
        .map_err(|e| format!("failed to start emulator: {e}"))?;
    if let Some(job) = JobGuard::new(&child) {
        SETUP_JOBS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(job);
    }
    Ok(())
}

// ------------------------------------------------------------------
// Input tunnel — the emulator window is parked invisible/offscreen,
// so clicks and keys can't reach it via SendInput/focus; the tunnel
// maps the on-screen capture rect to client coords and PostMessages
// the events straight into its WndProc instead.
// ------------------------------------------------------------------
/// egui::Key → Win32 virtual-key code. Punctuation maps to the OEM
/// VK the char lives on — the *shifted* char itself arrives through
/// the tunneled WM_CHAR stream, so the VK only needs the right key.
pub fn key_vk(key: egui::Key) -> Option<u16> {
    use egui::Key as K;
    // Letter/digit/F-key runs are contiguous in the enum — range
    // arithmetic on the discriminant keeps the table compact.
    let d = key as u32;
    let between = |lo: K, hi: K| (lo as u32..=hi as u32).contains(&d);
    if between(K::A, K::Z) {
        return Some(0x41 + (d - K::A as u32) as u16);
    }
    if between(K::Num0, K::Num9) {
        return Some(0x30 + (d - K::Num0 as u32) as u16);
    }
    if between(K::F1, K::F20) {
        return Some(0x70 + (d - K::F1 as u32) as u16);
    }
    Some(match key {
        K::ArrowDown => 0x28,
        K::ArrowLeft => 0x25,
        K::ArrowRight => 0x27,
        K::ArrowUp => 0x26,
        K::Tab => 0x09,
        K::Backspace => 0x08,
        K::Enter => 0x0D,
        K::Space => 0x20,
        K::Insert => 0x2D,
        K::Delete => 0x2E,
        K::Home => 0x24,
        K::End => 0x23,
        K::PageUp => 0x21,
        K::PageDown => 0x22,
        K::Colon | K::Semicolon => 0xBA,     // OEM_1 (;:)
        K::Plus | K::Equals => 0xBB,       // OEM_PLUS (=+)
        K::Comma => 0xBC,
        K::Minus => 0xBD,
        K::Period => 0xBE,
        K::Slash | K::Questionmark => 0xBF, // OEM_2 (/?)
        K::Backtick => 0xC0,
        K::OpenBracket | K::OpenCurlyBracket => 0xDB,
        K::Backslash | K::Pipe => 0xDC,
        K::CloseBracket | K::CloseCurlyBracket => 0xDD,
        K::Quote => 0xDE,
        K::Exclamationmark => 0x31, // shift+1 — the char rides WM_CHAR
        _ => return None,
    })
}

/// WM_KEYDOWN/UP lParam: repeat count 1, the real scancode, the
/// extended bit for the nav cluster/arrows, and the state bits
/// (previous-state for repeats, transition for releases). WndProcs
/// that only read wParam (Xenia's ImGui) don't care, but proper
/// lParams keep anyone who does honest.
pub fn key_lparam(vk: u16, pressed: bool, repeat: bool) -> LPARAM {
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
    let ext = matches!(
        vk,
        0x21..=0x28 | 0x2D | 0x2E
    ) as isize;
    let mut l = 1isize | ((sc as isize) << 16) | (ext << 24);
    if pressed && repeat {
        l |= 1 << 30;
    }
    if !pressed {
        l |= 0xC000_0000;
    }
    LPARAM(l)
}

/// WM_KEYDOWN for a held modifier while the tunnel runs — Xenia's UI
/// tracks Shift/Ctrl/Alt via their own key messages, so transitions
/// post too.
pub fn sync_mods(session: &Session, m: &egui::Modifiers, cur: &mut u8) {
    let want = m.shift as u8 | (m.ctrl as u8) << 1 | (m.alt as u8) << 2;
    for (bit, vk) in [(0x01u8, 0x10u16), (0x02, 0x11), (0x04, 0x12)] {
        if want & bit != *cur & bit {
            let down = want & bit != 0;
            session.post(
                if down { WM_KEYDOWN } else { WM_KEYUP },
                WPARAM(vk as usize),
                key_lparam(vk, down, false),
            );
        }
    }
    *cur = want;
}
