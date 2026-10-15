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
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, TerminateProcess,
    WaitForSingleObject, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
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
/// `temp_d3d_window` child; Flycast/xemu's is the main SDL window;
/// RPCS3's is the Qt `gs_frame` window).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Xenia,
    Flycast,
    Xemu,
    Rpcs3,
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
        match kind {
            // RPCS3: --no-gui skips the launcher/game-list shell so
            // the gs_frame render window is the only surface (Lacrima's
            // Rpcs3Handler.BuildStartInfo). Folder ROMs boot via their
            // EBOOT.BIN (Ps3InstalledGameHelper.GetPreferredBootPath).
            Kind::Rpcs3 => {
                ensure_rpcs3_pause_fix(&work_dir);
                cmd.arg("--no-gui").arg(ps3_boot_path(game));
            }
            // xemu mounts disc images via -dvd_path (XemuHandler's
            // BuildStartInfo); other file types pass bare.
            Kind::Xemu
                if matches!(
                    game.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_lowercase())
                        .as_deref(),
                    Some("iso") | Some("xiso")
                ) =>
            {
                cmd.arg("-dvd_path").arg(game);
            }
            _ => {
                cmd.arg(game);
            }
        }
        if let Some(xuid) = profile_xuid.filter(|_| kind == Kind::Xenia) {
            cmd.arg(format!("--logged_profile_slot_0_xuid={xuid:016X}"));
        }
        if kind == Kind::Xemu {
            // The captured window is hidden and unfocused — xemu must
            // capture controller input in the background or the game
            // gets no pad input at all (Lacrima's
            // EnsureBackgroundInputCaptureEnabled). Same mechanism
            // hides the in-frame menubar and the first-run welcome.
            ensure_xemu_settings(&work_dir);
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

/// Launcher-managed xemu.toml settings — patched next to the exe on
/// every launch (Lacrima's EnsureBackgroundInputCaptureEnabled,
/// extended). `background_input_capture` keeps the pad alive while
/// the window is hidden/unfocused; `show_menubar` hides xemu's
/// in-frame ImGui menubar (unreachable through capture anyway);
/// `show_welcome` skips the first-run welcome screen, which would
/// stall a launch on an unclickable dialog.
const XEMU_SETTINGS: &[(&str, &str, &str)] = &[
    ("input", "background_input_capture", "true"),
    ("display.ui", "show_menubar", "false"),
    ("general", "show_welcome", "false"),
];

/// Set `section.key = value` in xemu.toml — replaces the key in place
/// when it exists inside its section, inserts it right after the
/// section header, or appends `[section]\nkey = value` at EOF.
fn toml_set(content: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let mut out = String::with_capacity(content.len() + 64);
    let mut in_sec = false;
    let mut wrote = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            // Leaving the target section without having seen the key
            // — insert just before the next header.
            if in_sec && !wrote {
                out.push_str(&format!("{key} = {value}\n"));
                wrote = true;
            }
            in_sec = t.eq_ignore_ascii_case(&header);
        } else if in_sec
            && !wrote
            && t.find('=')
                .is_some_and(|eq| t[..eq].trim().eq_ignore_ascii_case(key))
        {
            out.push_str(&format!("{key} = {value}\n"));
            wrote = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !wrote {
        if in_sec {
            // The section runs to EOF.
            out.push_str(&format!("{key} = {value}\n"));
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{header}\n{key} = {value}\n"));
        }
    }
    out
}

/// Apply `XEMU_SETTINGS` to `xemu.toml` next to the launcher —
/// created when absent (xemu would write defaults over the top).
fn ensure_xemu_settings(launcher_dir: &Path) {
    let path = launcher_dir.join("xemu.toml");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut updated = content.clone();
    for (sec, key, val) in XEMU_SETTINGS {
        updated = toml_set(&updated, sec, key, val);
    }
    if updated != content {
        let _ = std::fs::write(&path, updated);
    }
}

/// Rpcs3Handler's EnsurePauseOnFocusLossDisabled: the captured window
/// is hidden/unfocused, so "Pause emulation on RPCS3 focus loss"
/// would freeze the game — force it off in config.yml (the GUI
/// section, `config/` subdir or launcher root). RPCS3 writes the file
/// itself on first run — only patch what exists.
fn ensure_rpcs3_pause_fix(launcher_dir: &Path) {
    const KEY: &str = "Pause emulation on RPCS3 focus loss";
    for path in [
        launcher_dir.join("config").join("config.yml"),
        launcher_dir.join("config.yml"),
    ] {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let updated: String = content
            .lines()
            .map(|l| {
                if l.contains(KEY) && l.contains(':') {
                    format!("  {KEY}: false")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if updated != content {
            let _ = std::fs::write(&path, updated);
        }
    }
}

/// Folder ROM → its EBOOT.BIN boot target
/// (Ps3InstalledGameHelper.GetPreferredBootPath); files pass through.
fn ps3_boot_path(game: &Path) -> PathBuf {
    if !game.is_dir() {
        return game.to_path_buf();
    }
    for dir in [
        game.to_path_buf(),
        game.join("PS3_GAME"),
        game.join("PS3_GAME").join("USRDIR"),
        game.join("USRDIR"),
    ] {
        for name in ["EBOOT.BIN", "eboot.bin"] {
            let p = dir.join(name);
            if p.is_file() {
                return p;
            }
        }
    }
    game.to_path_buf()
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
    //    xemu additionally requires the same candidate to win 3
    //    consecutive polls (ResolveCaptureTargetAsync's stable
    //    attempts) so a splash/transition window isn't captured, and
    //    gets Lacrima's 1500ms CaptureStartupDelay before we start
    //    looking — its early boot windows aren't the render surface.
    //
    //    HideUntilCaptured: every top-level window the process spawns
    //    is parked (offscreen, 0% opacity) on the poll after it
    //    appears — ~50ms of visibility instead of the whole resolve
    //    window. `hidden` keeps each window's original state for the
    //    capture-failure restore path.
    let mut hidden: std::collections::HashMap<usize, SavedWindow> =
        std::collections::HashMap::new();
    // Lacrima's CaptureStartupDelayMs — xemu waits 1500ms for its
    // early boot windows, RPCS3 2500ms (the gs_frame takes longer).
    // RPCS3 sweeps at 25ms instead of 100ms so a freshly-spawned Qt
    // window is cloaked within a frame or two — it must stay
    // invisible from spawn to capture.
    let delay = match kind {
        Kind::Xemu => 1500,
        Kind::Rpcs3 => 2500,
        _ => 0,
    };
    let poll = if kind == Kind::Rpcs3 { 25 } else { 100 };
    let mut waited = 0u64;
    while waited < delay {
        sweep_hide(pid, HWND::default(), &mut hidden, kind);
        if stop.load(Ordering::Relaxed) || !alive(proc) {
            break;
        }
        std::thread::sleep(Duration::from_millis(poll));
        waited += poll;
    }
    let mut hwnd = HWND::default();
    let mut stable_hwnd = HWND::default();
    let mut stable_count = 0u32;
    // RPCS3's resolve state: the window's last stable client size and
    // whether its 16:9 capture geometry has been applied yet.
    let mut rpcs3_geom = false;
    let mut rpcs3_size = (0i32, 0i32);
    // 25ms polls for RPCS3 keep the cloak gap sub-frame; other kinds
    // sweep at 50ms (window flash isn't an issue once captured).
    let poll = if kind == Kind::Rpcs3 { 25 } else { 50 };
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
        sweep_hide(pid, HWND::default(), &mut hidden, kind);
        hwnd = resolve_capture_target(pid, kind);
        if !hwnd.is_invalid() {
            if kind == Kind::Xemu {
                stable_count = if hwnd == stable_hwnd {
                    stable_count + 1
                } else {
                    stable_hwnd = hwnd;
                    1
                };
                if stable_count < 3 {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
            if kind == Kind::Rpcs3 {
                // ResolveCaptureTargetAsync: the candidate must reach
                // ≥960×540 before its 16:9 capture geometry is applied
                // once, then survive 10 polls at ≥1280×720 with an
                // unchanged size (Qt resizes gs_frame through boot).
                let (w, h) = client_size(hwnd).unwrap_or((0, 0));
                if !rpcs3_geom {
                    if w < 960 || h < 540 {
                        std::thread::sleep(Duration::from_millis(poll));
                        continue;
                    }
                    // Originals on record (cloak-snapshotted if the
                    // sweep already saw it), then strip decorations +
                    // apply the 16:9 size once — ApplyCaptureGeometryOnce.
                    hidden
                        .entry(hwnd.0 as usize)
                        .or_insert_with(|| {
                            let s = snapshot_window(hwnd);
                            set_cloaked(hwnd, true);
                            s
                        });
                    prepare_window_for_capture(hwnd);
                    apply_aspect_ratio(hwnd, 16.0 / 9.0);
                    rpcs3_geom = true;
                    stable_hwnd = HWND::default();
                    stable_count = 0;
                    rpcs3_size = (0, 0);
                    std::thread::sleep(Duration::from_millis(150));
                    continue;
                }
                if hwnd == stable_hwnd && (w, h) == rpcs3_size {
                    stable_count += 1;
                } else {
                    stable_hwnd = hwnd;
                    stable_count = 1;
                    rpcs3_size = (w, h);
                }
                if stable_count < 10 || w < 1280 || h < 720 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                apply_aspect_ratio(hwnd, 16.0 / 9.0);
            }
            hwnd_out.store(hwnd.0 as usize, Ordering::Relaxed);
            break;
        }
        stable_count = 0;
        stable_hwnd = HWND::default();
        std::thread::sleep(Duration::from_millis(poll));
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
    //    visible). A target already parked by the sweep keeps its
    //    ORIGINAL saved state — re-preparing would snapshot the
    //    stripped/hidden style and break the failure-path restore.
    let saved = hidden
        .remove(&(hwnd.0 as usize))
        .unwrap_or_else(|| prepare_window_for_capture(hwnd));
    hide_window(hwnd);
    strip_menu(hwnd);
    // Catch anything that appeared since the last poll.
    sweep_hide(pid, hwnd, &mut hidden, kind);

    // 3. Attach WGC — frames land in the shared slots (CPU mirror +
    //    zero-copy GPU ring).
    let _capture = wgc::Capture::start(hwnd, frames.clone(), gpu, render, low_latency);
    set_status(Status::Capturing);

    // 4. Monitor: stop request, process exit, or capture failure.
    //    The sweep keeps parking late windows (dialogs, consoles) the
    //    emulator creates after the target resolved.
    let mut capture_err: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) || !alive(proc) {
            break;
        }
        sweep_hide(pid, hwnd, &mut hidden, kind);
        // The capture target isn't swept — strip its menu too so a
        // late-attached menu bar can't leak into the captured frame.
        strip_menu(hwnd);
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
        for (h, s) in hidden {
            restore_window(HWND(h as *mut _), &s);
        }
        set_status(Status::Failed(err));
        if !proc.is_invalid() {
            unsafe {
                let _ = CloseHandle(proc);
            }
        }
        return;
    }

    // 5. Teardown — Lacrima's ForceCloseTrackedEmulatorProcess policy.
    //    RPCS3 gets its graceful stop shortcut first (TryRequestRpcs3-
    //    Shutdown): a WM_CLOSE pops the confirm-quit dialog on a
    //    hidden window and stalls shutdown. Everything else gets
    //    WM_CLOSE; whatever survives the grace wait is force-killed.
    //    (Lacrima's force-kill-first list — pcsx2/dolphin/duckstation/
    //    shadps4 — maps to the same branch when those kinds land.)
    if alive(proc) {
        let grace = if kind == Kind::Rpcs3 {
            send_ctrl_s(hwnd);
            // With --no-gui a landed Ctrl+S stops the game and exits
            // within ~a second; if it didn't land, don't sit in the
            // full 5s wait — cut to TerminateProcess sooner.
            2500
        } else {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            2500
        };
        let deadline = Instant::now() + Duration::from_millis(grace);
        while Instant::now() < deadline && alive(proc) {
            std::thread::sleep(Duration::from_millis(60));
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
    /// Hidden via DWM cloaking instead of park+opacity (RPCS3's
    /// render candidate — parking/restyling mid-construction races
    /// Qt, per Rpcs3Handler's Prepare*ForCapture no-ops).
    cloaked: bool,
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
    kind: Kind,
    best: HWND,
    best_score: i64,
}

unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut ChildCtx);
    let score = match ctx.kind {
        Kind::Rpcs3 => score_rpcs3_child(hwnd, ctx.pid),
        _ => score_render_child(hwnd, ctx.pid),
    };
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

/// IsLikelyXemuRenderWindow — xemu renders in its SDL top-level
/// window; dialog-ish titles (settings/monitor/about/…) are
/// rejected, xemu/xbox titles and SDL-class windows win, and a
/// non-primary top-level is accepted as the last resort.
fn is_likely_xemu_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    const BAD: &[&str] = &[
        "settings",
        "monitor",
        "about",
        "input",
        "controller",
        "network",
        "display",
    ];
    let title = window_title(hwnd);
    let t = title.trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    if !t.is_empty() {
        if BAD.iter().any(|w| t.contains(w)) {
            return false;
        }
        if t.contains("xemu") || t.contains("xbox") {
            return true;
        }
    }
    if !class.is_empty() && (class.contains("sdl") || class.contains("xemu")) {
        return hwnd != main || !t.is_empty();
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let has_thick = (style & WS_THICKFRAME.0 as i64) == WS_THICKFRAME.0 as i64;
    !(hwnd == main && has_caption && has_thick)
}

/// A window's client size in pixels (GetClientRect).
fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut r = RECT::default();
    unsafe { GetClientRect(hwnd, &mut r) }.ok()?;
    Some((r.right - r.left, r.bottom - r.top))
}

/// IsRpcs3DialogOrOverlayWindow — boot progress/trophy/shader
/// dialogs that must never be picked as the render surface.
fn is_rpcs3_dialog_title(t: &str) -> bool {
    [
        "trophy",
        "checking",
        "please wait",
        "please do not turn off",
        "do not turn off",
        "pipeline object",
        "compiling pipeline",
        "loading pipeline",
        "shader",
        "preloading",
        "installing",
    ]
    .iter()
    .any(|w| t.contains(w))
}

/// IsRpcs3UiWindow — RPCS3's own chrome (settings/log/launcher),
/// not a game surface.
fn is_rpcs3_ui_title(t: &str) -> bool {
    t.starts_with("rpcs3 ")
        || [
            "settings",
            "configuration",
            "controller",
            "input",
            "audio",
            "video",
            "about",
            "help",
            "log",
            "debug",
            "game list",
            "launcher",
            "master",
            "alpha",
        ]
        .iter()
        .any(|w| t.contains(w))
}

/// IsLikelyRpcs3RenderWindow — the gs_frame game window: a
/// non-child window ≥480×270 (client) that isn't dialog/UI chrome.
/// Render-ish titles (FPS:/vulkan/opengl/…) accept outright;
/// otherwise a non-primary Qt/QWindow top-level with a title.
fn is_likely_rpcs3_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let title = window_title(hwnd);
    let t = title.trim().to_lowercase();
    if is_rpcs3_dialog_title(&t) {
        return false;
    }
    let class = class_name(hwnd).to_lowercase();
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    if (style & WS_CHILD.0 as i64) != 0 {
        return false;
    }
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let mut looks_primary = hwnd == main;
    if is_rpcs3_ui_title(&t) {
        looks_primary = true;
    }
    let Some((w, h)) = client_size(hwnd) else {
        return false;
    };
    if w < 480 || h < 270 {
        return false;
    }
    if t.starts_with("fps:")
        || t.contains("vulkan")
        || t.contains("opengl")
        || t.contains("render")
        || t.contains("gpu")
        || t.contains("swapchain")
    {
        return true;
    }
    if t.contains("rpcs3") && !looks_primary && !has_caption {
        return true;
    }
    // gs_frame: a separate QWindow titled after the game.
    if !looks_primary
        && !t.is_empty()
        && (class.contains("qt") || class.contains("qwindow"))
    {
        return true;
    }
    !looks_primary && w >= 960 && h >= 540 && !t.is_empty()
}

/// ScoreRpcs3RenderChildWindow — gs_frame can also hang off the main
/// window as a Qt child; same gates as the top-level path with
/// render-class bonuses.
fn score_rpcs3_child(hwnd: HWND, pid: u32) -> i64 {
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
    let t = window_title(hwnd).trim().to_lowercase();
    if is_rpcs3_dialog_title(&t) {
        return i64::MIN;
    }
    let Some((w, h)) = client_size(hwnd) else {
        return i64::MIN;
    };
    if w < 480 || h < 270 {
        return i64::MIN;
    }
    let class = class_name(hwnd).to_lowercase();
    if class.contains("ime") || class.contains("tooltip") || class.contains("titlebar") {
        return i64::MIN;
    }
    let mut score = w as i64 * h as i64 * 10;
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        score += 500_000;
    }
    if class.contains("qt") || class.contains("vulkan") || class.contains("opengl") {
        score += 5_000_000;
    }
    if w >= 1280 && h >= 720 {
        score += 25_000_000;
    }
    score
}

/// IsLikelyXeniaRenderWindow — heuristic for top-level candidates
/// when no dedicated render child exists.
fn is_likely_render_window(hwnd: HWND, main: HWND, kind: Kind) -> bool {
    match kind {
        Kind::Flycast => return is_likely_flycast_window(hwnd, main),
        Kind::Xemu => return is_likely_xemu_window(hwnd, main),
        Kind::Rpcs3 => return is_likely_rpcs3_window(hwnd, main),
        Kind::Xenia => {}
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
    // Flycast/xemu require a real client area before assign
    // (IsStableCaptureCandidate — the UI settles at ≥640×360).
    if kind != Kind::Xenia && (w < 640 || h < 360) {
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
    // lives under a top-level) and RPCS3 (gs_frame can be a Qt child
    // of the main window); Flycast/xemu render in the top-level
    // window itself — a child could only be chrome, so skip.
    if matches!(kind, Kind::Xenia | Kind::Rpcs3) {
        let mut ctx = ChildCtx {
            pid,
            kind,
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
        cloaked: false,
    }
}

/// Read-only window snapshot — the same restore record
/// `prepare_window_for_capture` makes, but without touching the
/// window (for the cloak-only hide path).
fn snapshot_window(hwnd: HWND) -> SavedWindow {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let exstyle = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as i64;
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    SavedWindow {
        style,
        exstyle,
        rect,
        menu: None,
        cloaked: true,
    }
}

/// Win32API.SetWindowCloaked (DwmSetWindowAttribute DWMWA_CLOAK=13):
/// hides a window from the screen without touching its geometry,
/// style or opacity — and DWM/WGC keep producing frames for it. The
/// hide that can't race a still-constructing render window.
fn set_cloaked(hwnd: HWND, on: bool) {
    let v: BOOL = on.into();
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(13),
            &v as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
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
    if saved.cloaked {
        set_cloaked(hwnd, false);
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

/// PrepareProcessForCapture on a sweep cadence — prepare + hide every
/// top-level window of the process not already parked, except `skip`
/// (the resolved capture target, whose original state lives in
/// `saved`). Hidden windows stay `WS_VISIBLE` (opacity/position only)
/// so they keep enumerating — the map dedups repeat visits.
///
/// RPCS3's likely render window is *cloaked* instead of parked:
/// Rpcs3Handler deliberately no-ops PrepareProcessForCapture/
/// PrepareWindowForCapture because moving/restyling a gs_frame mid-
/// construction races Qt and produces stale aspect ratios. Cloaking
/// hides it without touching geometry — the real prepare+park runs
/// once the window proves stable.
fn sweep_hide(
    pid: u32,
    skip: HWND,
    hidden: &mut std::collections::HashMap<usize, SavedWindow>,
    kind: Kind,
) {
    for hwnd in process_windows(pid, false) {
        if hwnd == skip || hwnd.is_invalid() {
            continue;
        }
        // A real HMENU can be attached after the window's first
        // sweep, so re-strip on every pass. (xemu's menubar isn't an
        // HMENU at all — it's ImGui-rendered in-frame, handled by the
        // show_menubar config patch.)
        strip_menu(hwnd);
        // RPCS3: cloak EVERY window — a DWM cloak hides instantly with
        // no geometry/style change for Qt to race, so nothing flashes
        // between spawn and capture. Other kinds park aux windows.
        let cloak = kind == Kind::Rpcs3;
        hidden
            .entry(hwnd.0 as usize)
            .or_insert_with(|| {
                if cloak {
                    let s = snapshot_window(hwnd);
                    set_cloaked(hwnd, true);
                    s
                } else {
                    let saved = prepare_window_for_capture(hwnd);
                    hide_window(hwnd);
                    saved
                }
            });
    }
}

/// Lacrima's TrySendControlS — RPCS3's graceful stop shortcut. Focus
/// the window then push a real Ctrl+S through SendInput; posted
/// WM_KEYDOWNs don't reach Qt's shortcut map on an inactive window.
/// The window stays cloaked — SetForegroundWindow doesn't undo a DWM
/// cloak, so nothing flashes while it stops.
fn send_ctrl_s(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    let key = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                ..Default::default()
            },
        },
    };
    unsafe {
        // SetForegroundWindow quietly fails when the caller isn't the
        // foreground process — attaching to the target's input queue
        // sidesteps the restriction so the shortcut actually lands
        // instead of burning the whole grace period.
        let target_tid = GetWindowThreadProcessId(hwnd, None);
        let this_tid = GetCurrentThreadId();
        let attached = target_tid != 0
            && target_tid != this_tid
            && AttachThreadInput(this_tid, target_tid, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        // Ctrl down, S down, S up, Ctrl up.
        let inputs = [
            key(0x11, false),
            key(0x53, false),
            key(0x53, true),
            key(0x11, true),
        ];
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if attached {
            let _ = AttachThreadInput(this_tid, target_tid, false);
        }
    }
}

/// Remove a window's menu bar if it has one — a no-op when there
/// isn't (cheap enough to run on every sweep).
fn strip_menu(hwnd: HWND) {
    unsafe {
        if !GetMenu(hwnd).is_invalid() {
            let _ = SetMenu(hwnd, None);
            let _ = DrawMenuBar(hwnd);
        }
    }
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
