//! The Xenia (Xbox 360) emulator handler — a port of Lacrima's
//! `XeniaHandler` + the handler-base window plumbing. Launches
//! `xenia.exe <rom>`, resolves the game's render window (Xenia draws
//! into a `temp_d3d_window` child rather than its main window),
//! strips decorations, parks it at the far corner of the virtual
//! desktop at 0% opacity — still visible to DWM so Windows Graphics
//! Capture keeps producing frames — and hands the HWND to `wgc`.
//!
//! On non-Windows platforms there is no HWND/WGC embedding — the
//! session spawns the emulator as an external process instead. The
//! launch/args/config-seeding logic is shared.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::thread::JoinHandle;
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use windows::core::BOOL;
#[cfg(windows)]
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_TIMEOUT, WPARAM,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
#[cfg(windows)]
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleWindow, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::*;

#[cfg(windows)]
use eframe::egui;

use crate::wgc;
#[cfg(not(windows))]
use crate::wlcomp;
#[cfg(not(windows))]
use crate::xenia_cfg;

/// Launch/session diagnostics — one timestamped line per event to
/// `<data_root>/session.log` (truncated at 256KB). The failure modes
/// a dead relaunch can hide in — launch() early-returns, spawn
/// errors, worker transitions — leave no trace otherwise.
pub fn session_log(msg: &str) {
    let Some(root) = crate::settings::data_root() else {
        return;
    };
    let _ = std::fs::create_dir_all(&root);
    let p = root.join("session.log");
    if std::fs::metadata(&p)
        .map(|m| m.len() > 256 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&p);
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        use std::io::Write;
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

// Lacrima's PauseEmulator/ResumeEmulator — the ntdll suspend pair
// (OpenProcess + NtSuspendProcess / NtResumeProcess).
#[cfg(windows)]
#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: *mut core::ffi::c_void) -> i32;
    fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
}

#[cfg(windows)]
const SUSPEND_ACCESS: PROCESS_ACCESS_RIGHTS =
    PROCESS_ACCESS_RIGHTS(PROCESS_SUSPEND_RESUME.0 | PROCESS_QUERY_LIMITED_INFORMATION.0);

/// Owns a Windows Job Object with KILL_ON_JOB_CLOSE: when the last
/// handle to the job closes — on normal Drop AND on an abrupt process
/// death where destructors never run (panic=abort, crash, kill) — the
/// OS terminates every process assigned to it. This is what keeps a
/// launched emulator from being orphaned when the app dies.
#[cfg(windows)]
struct JobGuard(HANDLE);

// A job handle is a process-global kernel handle — safe to move
// across threads.
#[cfg(windows)]
unsafe impl Send for JobGuard {}

#[cfg(windows)]
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

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Session lifecycle shared with the UI thread.
#[derive(Clone, Debug)]
pub enum Status {
    /// Searching the process tree for the render window.
    Resolving,
    /// Capture session attached — frames are flowing.
    Capturing,
    /// Setup failed (no window, WGC attach error, …).
    /// Off Windows spawn errors surface as start() Err instead.
    #[cfg_attr(not(windows), allow(dead_code))]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Xenia,
    Flycast,
    Xemu,
    Rpcs3,
    Shadps4,
    DuckStation,
    Pcsx2,
    Dolphin,
    Cemu,
    Eden,
    RetroArch,
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
    /// The nested Wayland compositor hosting the emulator — `Some`
    /// while the session renders into our private socket, `None`
    /// when the launch fell back to an external desktop window.
    /// Dropping it closes the socket; `stop()` already killed the
    /// process group, so drop order vs. `child` doesn't matter.
    #[cfg(not(windows))]
    compositor: Option<wlcomp::WlComp>,
    /// The captured render window — written by the worker once the
    /// capture target resolves; the Aspect-Ratio context menu resizes
    /// it (Lacrima's EmulatorTargetHwnd).
    #[cfg(windows)]
    hwnd: Arc<std::sync::atomic::AtomicUsize>,
    /// The resolve/capture worker (Windows only — Linux sessions run
    /// the emulator in its own window, nothing to resolve).
    #[cfg(windows)]
    worker: Option<JoinHandle<()>>,
    /// Kill-on-close job containing the emulator — keeps it from
    /// being orphaned even by a hard crash. Declared last so it is
    /// the final field to drop.
    #[cfg(windows)]
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
    /// `retroarch_core` (RetroArch kind only) is the selected
    /// Libretro core library RetroArch loads via `-L`.
    /// On non-Windows the emulator runs in its own window — no WGC
    /// capture, no window hiding, no input tunnel.
    #[cfg_attr(not(windows), allow(unused_variables))]
    pub fn start(
        exe: &str,
        game: &Path,
        low_latency: bool,
        profile_xuid: Option<u64>,
        kind: Kind,
        retroarch_core: Option<PathBuf>,
    ) -> Result<Self, String> {
        session_log(&format!(
            "Session::start kind={kind:?} exe={exe} game={}",
            game.display()
        ));
        let exe_path = eden_cli_sibling(exe, kind);
        let work_dir = exe_path
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_default();
        // shadPS4's stdout goes to a launch transcript the worker
        // scans for [Core.Linker] before resolving the render window
        // (WaitForCoreLinkerOutputAsync).
        let mut transcript: Option<PathBuf> = None;
        let mut cmd = Command::new(&exe_path);
        // Bundled-lib shim dir: `<exe>/lib` is searched first when it
        // exists — covers distro gaps like libGLU (Cemu) without
        // needing system packages.
        #[cfg(not(windows))]
        {
            let lib = work_dir.join("lib");
            if lib.is_dir() {
                cmd.env("LD_LIBRARY_PATH", &lib);
            }
        }
        match kind {
            // RPCS3: --no-gui skips the launcher/game-list shell so
            // the gs_frame render window is the only surface (Lacrima's
            // Rpcs3Handler.BuildStartInfo). Folder ROMs boot via their
            // EBOOT.BIN (Ps3InstalledGameHelper.GetPreferredBootPath).
            Kind::Rpcs3 => {
                ensure_rpcs3_pause_fix(&work_dir);
                cmd.arg("--no-gui").arg(ps3_boot_path(game));
            }
            // shadPS4: `-g <game> -f false` — the launcher would ignore
            // the game path (ResolveShadPs4ExecutablePath upgrades a
            // qtlauncher selection to the sibling core exe). The user
            // dir is forced portable (`<exe dir>/user` gets SHADPS4_
            // USER_DIR + APPDATA/LOCALAPPDATA/XDG_*) so patches and
            // cheats land in a known place, and stdout/stderr go to a
            // transcript the worker scans for [Core.Linker] — Lacrima's
            // WaitForCoreLinkerOutput gate before capture resolution.
            Kind::Shadps4 => {
                let user = crate::shadps4::user_dir(&work_dir);
                let _ = std::fs::create_dir_all(&user);
                crate::shadps4::ensure_background_input(&work_dir);
                cmd.env("SHADPS4_USER_DIR", &user)
                    .env("APPDATA", &user)
                    .env("LOCALAPPDATA", &user)
                    .env("XDG_CONFIG_HOME", &user)
                    .env("XDG_DATA_HOME", &user);
                cmd.arg("-g")
                    .arg(crate::shadps4::resolve_game_path(game))
                    .arg("-f")
                    .arg("false");
                let log_dir = user.join("log");
                let _ = std::fs::create_dir_all(&log_dir);
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let tp = log_dir.join(format!("shadps4_launch_{nanos:x}.log"));
                if let Ok(out) = std::fs::File::create(&tp) {
                    if let Ok(err) = out.try_clone() {
                        cmd.stdout(std::process::Stdio::from(out))
                            .stderr(std::process::Stdio::from(err));
                        transcript = Some(tp);
                    }
                }
                // shadPS4's exe is console-subsystem — spawned from a
                // GUI process Windows allocates it a console window
                // (the "command window"). CREATE_NO_WINDOW suppresses
                // the allocation; the transcript above keeps the log.
                // (Lacrima achieves the same via `cmd /d /c` +
                // CreateNoWindow.)
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                }
            }
            // DuckStation: `-batch -nofullscreen -nogui -- <rom>` —
            // switches before `--`, the image path after so the
            // filename can't parse as an option (DuckStationHandler's
            // BuildStartInfo). portable.txt forces portable mode
            // (settings.ini beside the exe) and PauseOnFocusLoss must
            // be off — a hidden window never gets focus.
            Kind::DuckStation => {
                ensure_duckstation_setup(&work_dir);
                cmd.arg("-batch")
                    .arg("-nofullscreen")
                    .arg("-nogui")
                    .arg("--")
                    .arg(game);
            }
            // PCSX2: `-batch -nogui -portable <rom>` — no `--`
            // separator (Pcsx2Handler's BuildStartInfo); -portable
            // keeps `inis/` beside the exe. PauseOnFocusLoss is
            // forced off — a hidden window never gets focus.
            Kind::Pcsx2 => {
                ensure_pcsx2_pause_fix(&work_dir);
                cmd.arg("-batch").arg("-nogui").arg("-portable").arg(game);
            }
            // Dolphin: `-u <exe dir>/User -b -e <rom>`
            // (DolphinHandler's BuildStartInfo) — -u forces the
            // portable user dir under the managed install, -b is
            // batch mode (quit with the game). No -f: the window is
            // hidden/cloaked anyway. PauseOnFocusLost/SkipNKitWarning/
            // EnableCheats land in the portable Dolphin.ini first —
            // a hidden window never gets focus.
            Kind::Dolphin => {
                let user = ensure_dolphin_setup(&work_dir);
                cmd.arg("-u").arg(&user).arg("-b").arg("-e").arg(game);
            }
            // Cemu: `-g <rom>` (CemuHandler's BuildStartInfo minus
            // the Cemu 1.x `-c/-mlc` flags — 2.x's LaunchSettings has
            // no -c and parses `-mlc` as `-m lc`). Portable mode in
            // 2.x is runtime-detected: a `portable/` dir next to the
            // exe redirects settings.xml/log.txt/mlc01 into it
            // (CemuPathsService's same check). No -f: the window is
            // hidden/cloaked anyway, and the fullscreen scaling
            // workaround only matters there. The transcript carries
            // the exe dir — the worker watches portable/log.txt +
            // log.txt for "------- Run title -------"
            // (WaitForRenderReadyLogAsync / ResolveCemuLogFilePath).
            Kind::Cemu => {
                ensure_cemu_setup(&work_dir);
                transcript = Some(work_dir.clone());
                // -f launches the game fullscreen — hides the wx
                // menubar in the compositor (Windows stays windowed:
                // its window is cloaked anyway).
                #[cfg(not(windows))]
                cmd.arg("-f");
                cmd.arg("-g").arg(game);
            }
            // Eden (EdenHandler.BuildStartInfo): the cli frontend
            // gets `--config <qt-config.ini> -g <rom>`; the Qt gui
            // exe just `-g <rom>` (no -f — the window is cloaked
            // anyway). ensure_eden_setup writes the qt-config.ini
            // overrides first (fullscreen off, status bar off,
            // pause_on_focus_loss off — a hidden window never has
            // focus). stdout/stderr go to a transcript the worker
            // scans for "RenderReady" (WaitForRenderReadyOutputAsync)
            // — only the cli emits it — and CREATE_NO_WINDOW keeps
            // the console-subsystem exe from flashing a cmd window.
            Kind::Eden => {
                ensure_eden_setup();
                #[cfg(not(windows))]
                {
                    // Isolate Qt config into the managed XDG dir so
                    // Eden writes qt-config.ini/window_state.ini there
                    // — never beside the app or under ~/.config.
                    let xdg = eden_xdg_dir();
                    let _ = std::fs::create_dir_all(&xdg);
                    cmd.env("XDG_CONFIG_HOME", &xdg);
                }
                let stem = exe_path
                    .file_stem()
                    .and_then(|f| f.to_str())
                    .unwrap_or_default()
                    .to_lowercase();
                if stem.contains("eden-cli") || stem.contains("edencli") {
                    cmd.arg("--config").arg(eden_config_path());
                    let log_dir = eden_config_dir();
                    let _ = std::fs::create_dir_all(&log_dir);
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let tp = log_dir.join(format!("eden_launch_{nanos:x}.log"));
                    if let Ok(out) = std::fs::File::create(&tp) {
                        if let Ok(err) = out.try_clone() {
                            cmd.stdout(std::process::Stdio::from(out))
                                .stderr(std::process::Stdio::from(err));
                            transcript = Some(tp);
                        }
                    }
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                    }
                }
                #[cfg(not(windows))]
                {
                    // Qt hides menu/status chrome only in fullscreen —
                    // the ini key lags (Eden rewrites it on exit), so
                    // the flag is authoritative.
                    cmd.arg("-f");
                }
                cmd.arg("-g").arg(game);
            }
            // RetroArch (RetroArchHandler.BuildRetroArchStartInfo):
            // --appendconfig <focus overrides> -L <core> <rom>. The
            // core is the managed 3DS Libretro download the user
            // picked; no --fullscreen — the window is hidden/cloaked
            // like the other launches.
            Kind::RetroArch => {
                let core = retroarch_core.as_ref().ok_or_else(|| {
                    "No 3DS core installed — press Download all cores in Settings → Emulation"
                        .to_string()
                })?;
                // Linux: RA ignores the compositor-driven fullscreen
                // configure (commits a windowed-size surface) —
                // request fullscreen client-side so it fills the
                // nested output. Windows stays windowed (WGC captures
                // the hidden window).
                #[cfg(not(windows))]
                cmd.arg("--fullscreen");
                if let Some(cfg_path) =
                    retroarch_append_config(&exe_path, retroarch_core.as_deref())
                {
                    cmd.arg("--appendconfig").arg(cfg_path);
                }
                if let Some(log_path) = retroarch_log_path(retroarch_core.as_deref()) {
                    cmd.arg("--verbose").arg("--log-file").arg(&log_path);
                    // The worker's resolve-failure read pulls the
                    // last [ERROR] line from this file — per-section,
                    // like the cores themselves.
                    transcript = Some(log_path);
                }
                cmd.arg("-L").arg(core).arg(game);
                // Never inherit our std handles — after the
                // watchdog's AttachConsole/FreeConsole cycle ours
                // can be dangling, and CreateProcess fails the spawn
                // outright with ERROR_INVALID_HANDLE (os error 6).
                cmd.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                // Lacrima's CreateNoWindow — "hides the console log
                // window". --verbose makes RA AllocConsole() when the
                // parent has none; the flag keeps that console
                // invisible instead of flashing a cmd window.
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                }
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
            // Xenia on Linux: --fullscreen hides the ImGui menu bar
            // (the nested compositor is a real display it can
            // fullscreen onto — Windows stays windowed since the window
            // is cloaked anyway), and --storage_root aims the whole
            // storage (config, content, profiles) at our managed dir —
            // without it the AppImage is non-portable and reads
            // ~/.local/share/Xenia, so our config overrides and profile
            // writes would never reach it.
            Kind::Xenia => {
                #[cfg(not(windows))]
                {
                    cmd.arg("--fullscreen");
                    if let Some(root) =
                        xenia_cfg::storage_root(exe_path.parent())
                    {
                        cmd.arg(format!("--storage_root={}", root.display()));
                    }
                }
                cmd.arg(game);
            }
            _ => {
                cmd.arg(game);
            }
        }
        if let Some(xuid) = profile_xuid.filter(|_| kind == Kind::Xenia) {
            cmd.arg(format!("--logged_profile_slot_0_xuid={xuid:016X}"));
        }
        if kind == Kind::Xemu {
            // Linux: -full-screen makes xemu fullscreen onto the nested
            // compositor's output — its display fills the captured
            // frame instead of staying a windowed layout with the
            // canvas inset under the menubar (it ignored our
            // compositor-initiated fullscreen configure). Windows
            // stays windowed — WGC captures the hidden window.
            #[cfg(not(windows))]
            cmd.arg("-full-screen");
            // The captured window is hidden and unfocused — xemu must
            // capture controller input in the background or the game
            // gets no pad input at all (Lacrima's
            // EnsureBackgroundInputCaptureEnabled). Same mechanism
            // hides the in-frame menubar and the first-run welcome.
            ensure_xemu_settings(&work_dir);
        }
        // Own process group — stop() can kill the whole tree on unix
        // (emulators spawn helper processes), matching the Windows
        // kill-on-close job guard.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        // Linux: bring up the nested compositor and aim the emulator
        // at its private socket. `WAYLAND_DISPLAY` resolves against
        // XDG_RUNTIME_DIR, which the app already inherits. `DISPLAY`
        // is stripped so X11-capable builds still pick Wayland —
        // XWayland clients come later. On failure we fall back to the
        // old external-window mode and leave the env untouched.
        #[cfg(not(windows))]
        let frames = Arc::new(Mutex::new(wgc::FrameSlot::default()));
        #[cfg(not(windows))]
        let compositor = match wlcomp::WlComp::start(
            (1920, 1080),
            frames.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ) {
            Ok(c) => {
                session_log(&format!("wlcomp socket {:?}", c.socket_name()));
                // XWayland comes up async — wait briefly for its
                // display number so X11-only emulators can use it.
                // Xenia's Linux build has no wayland Vulkan surface at
                // all (xcb-only), so it goes straight to XWayland and
                // must NOT see WAYLAND_DISPLAY (its GTK would pick
                // wayland while its presenter needs X11).
                let x11 = c.wait_display(std::time::Duration::from_secs(4));
                match (kind, x11) {
                    (Kind::Xenia, Some(d)) => {
                        session_log(&format!("x11 display :{d} for {kind:?}"));
                        cmd.env("DISPLAY", format!(":{d}"));
                        cmd.env_remove("WAYLAND_DISPLAY");
                        // Force GTK's x11 backend — with WAYLAND_DISPLAY
                        // unset, libwayland still defaults to the host's
                        // wayland-0 if x11 fails, which would put the
                        // window on the user's desktop.
                        cmd.env("GDK_BACKEND", "x11");
                    }
                    (_, Some(d)) => {
                        cmd.env("WAYLAND_DISPLAY", c.socket_name());
                        cmd.env("DISPLAY", format!(":{d}"));
                    }
                    (_, None) => {
                        cmd.env("WAYLAND_DISPLAY", c.socket_name());
                        cmd.env_remove("DISPLAY");
                    }
                }
                // iGPU+dGPU desktops: Vulkan enumerates in PCI order
                // and `gpu=any`-style clients grab the APU's 2-CU iGPU
                // (xenia picked RAPHAEL_MENDOCINO over NAVI31). RADV
                // honors MESA_VK_DEVICE_SELECT to reorder its physical
                // device list toward the discrete card.
                if let Some(sel) = wlcomp::vulkan_device_select() {
                    cmd.env("MESA_VK_DEVICE_SELECT", sel);
                }
                Some(c)
            }
            Err(e) => {
                session_log(&format!("wlcomp start failed ({e}) — external window"));
                None
            }
        };
        // On Linux, route emulator stdout/stderr to a transcript —
        // without it, "Failed to initialize GTK+" and friends are
        // invisible. Skip kinds that already own a transcript
        // (shadPS4, RetroArch) and null-io RetroArch.
        #[cfg(not(windows))]
        if transcript.is_none() && kind != Kind::RetroArch {
            let log_path = crate::settings::data_root()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("emu-launch.log");
            if let Ok(f) = std::fs::File::create(&log_path) {
                if let Ok(err) = f.try_clone() {
                    cmd.stdout(std::process::Stdio::from(f))
                        .stderr(std::process::Stdio::from(err));
                }
            }
        }
        let child = cmd
            .current_dir(work_dir)
            .spawn()
            .map_err(|e| format!("failed to start emulator: {e}"))
            .inspect_err(|e| session_log(&format!("spawn failed: {e}")))?;
        session_log(&format!("spawned kind={kind:?} pid={}", child.id()));

        // Kill-on-close job FIRST — if anything below panics the
        // guard unwinds and the OS reaps the emulator for us.
        #[cfg(windows)]
        let job = JobGuard::new(&child);

        #[cfg_attr(not(windows), allow(unused_variables))]
        let pid = child.id();
        let status = Arc::new(Mutex::new(Shared {
            status: Status::Resolving,
        }));
        #[cfg(windows)]
        let frames = Arc::new(Mutex::new(wgc::FrameSlot::default()));
        let gpu = Arc::new(wgc::GpuState::default());
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let render = Arc::new(wgc::RenderShared::default());
        #[cfg(windows)]
        let hwnd = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(windows)]
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
                        worker_main(
                            pid,
                            status,
                            frames,
                            gpu,
                            stop,
                            render,
                            hwnd,
                            low_latency,
                            kind,
                            transcript,
                        )
                    }))
                    .is_err()
                    {
                        status2.lock().unwrap_or_else(|p| p.into_inner()).status =
                            Status::Failed("capture worker crashed".into());
                    }
                })
                .expect("spawn xenia worker")
        };
        // No window resolution or capture off Windows — the emulator
        // runs in its own window; the session layer reports it as
        // live for as long as the process is up.
        #[cfg(not(windows))]
        {
            let _ = transcript;
            status.lock().unwrap_or_else(|p| p.into_inner()).status = Status::Capturing;
        }
        Ok(Self {
            child,
            game: game.to_path_buf(),
            status,
            frames,
            gpu,
            stop,
            paused,
            render,
            #[cfg(not(windows))]
            compositor,
            #[cfg(windows)]
            hwnd,
            #[cfg(windows)]
            worker: Some(worker),
            #[cfg(windows)]
            _job: job,
        })
    }

    /// `true` when the emulator is hosted inside the nested
    /// compositor (frames land in `frames()`), `false` when it fell
    /// back to its own desktop window.
    #[cfg(not(windows))]
    pub fn embedded(&self) -> bool {
        self.compositor.is_some()
    }

    /// Windows sessions are always embedded — the emulator window is
    /// captured/hidden by definition, never an external desktop
    /// window. Exists so cross-platform call sites can gate on
    /// `embedded()` without cfg soup of their own.
    #[cfg(windows)]
    pub fn embedded(&self) -> bool {
        true
    }

    /// Forward a UI-originated input event into the nested seat —
    /// no-op when the session runs in an external window.
    #[cfg(not(windows))]
    pub fn send_input(&self, ev: wlcomp::WlInput) {
        if let Some(c) = &self.compositor {
            c.send_input(ev);
        }
    }

    /// Live render adjustments — write the atomics and the capture
    /// thread applies them on the very next frame.
    pub fn render(&self) -> &Arc<wgc::RenderShared> {
        &self.render
    }

    /// The captured render window — invalid until the worker resolves
    /// the capture target.
    #[cfg(windows)]
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
        // No capture worker off Windows — signal the whole process
        // group (the emulator plus helpers it spawned), then the
        // direct child as a fallback.
        #[cfg(not(windows))]
        {
            let pid = self.child.id() as i32;
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
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
    #[cfg(windows)]
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
    #[cfg(windows)]
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
    #[cfg(windows)]
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
    #[cfg(windows)]
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
    #[cfg(windows)]
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
        // Failed counts as finished too — a dead capture with the
        // emulator still running must end the session (Lacrima tears
        // the emulator down on capture failure); otherwise the game
        // runs forever, hidden and uncaptured.
        if status_finished(&self.status()) {
            return true;
        }
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            // No worker turns the status to Exited off Windows — set
            // it here so the final status shown is true.
            self.status
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .status = Status::Exited;
            return true;
        }
        false
    }
}

/// Session-status → session-over predicate (UI drops the session —
/// Drop reaps the child). `Failed` counts: a capture that can never
/// attach leaves the emulator running hidden/uncaptured otherwise.
fn status_finished(status: &Status) -> bool {
    matches!(status, Status::Exited | Status::Failed(_))
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Detach — the worker finishes teardown on its own.
        #[cfg(windows)]
        self.worker.take();
    }
}

/// OpenProcess + NtSuspend/Resume — returns false when the process is
/// gone or the call fails (UI keeps the un-paused state then).
#[cfg(windows)]
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

/// SIGSTOP/SIGCONT — the POSIX counterpart of the ntdll suspend pair.
#[cfg(not(windows))]
fn set_process_suspended(pid: u32, on: bool) -> bool {
    unsafe {
        libc::kill(
            pid as i32,
            if on { libc::SIGSTOP } else { libc::SIGCONT },
        ) == 0
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

/// DuckStationHandler's EnsurePortableModeMarker +
/// EnsurePauseOnFocusLossDisabled: `portable.txt` beside the exe
/// keeps all state in the install dir, and pause-on-focus-loss must
/// be off or the hidden window freezes the game.
fn ensure_duckstation_setup(exe_dir: &Path) {
    let _ = std::fs::write(exe_dir.join("portable.txt"), b"");
    let ini = exe_dir.join("settings.ini");
    let Ok(content) = std::fs::read_to_string(&ini) else {
        return;
    };
    let mut out = String::with_capacity(content.len());
    let mut dirty = false;
    for line in content.lines() {
        if line
            .trim_start()
            .to_lowercase()
            .starts_with("pauseonfocusloss")
            && line.contains('=')
        {
            if line.trim() != "PauseOnFocusLoss = false" {
                out.push_str("PauseOnFocusLoss = false");
                dirty = true;
            } else {
                out.push_str(line);
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if dirty {
        let _ = std::fs::write(&ini, out);
    }
}

/// Pcsx2Handler's EnsurePauseOnFocusLossDisabled — `-portable` puts
/// settings in `<exe dir>/inis/PCSX2.ini`; a hidden window never gets
/// focus, so pause-on-focus-loss would freeze the game.
fn ensure_pcsx2_pause_fix(exe_dir: &Path) {
    let ini = exe_dir.join("inis").join("PCSX2.ini");
    let Ok(content) = std::fs::read_to_string(&ini) else {
        return;
    };
    let mut out = String::with_capacity(content.len());
    let mut dirty = false;
    for line in content.lines() {
        if line
            .trim_start()
            .to_lowercase()
            .starts_with("pauseonfocusloss")
            && line.contains('=')
        {
            if line.trim() != "PauseOnFocusLoss = false" {
                out.push_str("PauseOnFocusLoss = false");
                dirty = true;
            } else {
                out.push_str(line);
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if dirty {
        let _ = std::fs::write(&ini, out);
    }
}

/// DolphinHandler's ApplyDolphinLaunchConfigOverrides — the portable
/// `-u` user dir under the managed install is the only config root we
/// touch (Lacrima also writes `Documents\Dolphin Emulator`; portable
/// mode keeps everything beside the exe). PauseOnFocusLost must be
/// off — a hidden window never gets focus.
fn ensure_dolphin_setup(exe_dir: &Path) -> PathBuf {
    let user = exe_dir.join("User");
    let ini = user.join("Config").join("Dolphin.ini");
    if let Some(d) = ini.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    set_ini_key(&ini, "Interface", "PauseOnFocusLost", "False");
    set_ini_key(&ini, "Interface", "SkipNKitWarning", "True");
    set_ini_key(&ini, "Core", "EnableCheats", "True");
    // Lacrima doesn't ship this one, but the capture window never
    // takes focus — without it Dolphin ignores the gamepad entirely
    // while unfocused (Controller settings → "Background Input").
    // Lives under [Input], not [Interface] (MAIN_INPUT_BACKGROUND_INPUT).
    set_ini_key(&ini, "Input", "BackgroundInput", "True");
    user
}

/// Cemu 2.x has no `-c` config flag — portable mode is detected at
/// runtime: a `portable/` directory (or settings.xml) beside
/// `Cemu.exe` redirects settings.xml/log.txt/mlc01/controllerProfiles
/// into it (CemuPathsService's ResolveUserDataDirectory). Creating
/// the dir is all the "setup" a launch needs.
fn ensure_cemu_setup(exe_dir: &Path) {
    let _ = std::fs::create_dir_all(exe_dir.join("portable"));
    #[cfg(not(windows))]
    {
        // Cemu's "Getting started" wizard only runs while settings.xml
        // is missing — an undismissable blocker under the compositor.
        // Seed the portable copy from the user's real config (game
        // paths + tuned settings carry over), or a minimal valid file.
        let settings = exe_dir.join("portable").join("settings.xml");
        if !settings.exists() {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
                .join("Cemu")
                .join("settings.xml");
            let mlc = exe_dir.join("portable").join("mlc01");
            let body = std::fs::read_to_string(&home).unwrap_or_else(|_| {
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<content>\n</content>\n"
                )
            });
            let body = if body.contains("<mlc_path></mlc_path>") {
                body.replace(
                    "<mlc_path></mlc_path>",
                    &format!("<mlc_path>{}</mlc_path>", mlc.display()),
                )
            } else {
                body
            };
            let _ = std::fs::write(&settings, body);
        }
    }
}

/// CemuHandler's ResolveCemuLogFilePath candidates for a spawn at
/// `exe_dir`: portable-mode log first (what our `-g` launch writes),
/// then the exe dir, then the %APPDATA% fallback for un-portable
/// configured exes.
#[cfg(windows)]
fn cemu_log_candidates(exe_dir: &Path) -> [PathBuf; 3] {
    [
        exe_dir.join("portable").join("log.txt"),
        exe_dir.join("log.txt"),
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Cemu")
            .join("log.txt"),
    ]
}

/// EdenHandler.BuildStartInfo — a configured `eden.exe` launches
/// its `eden-cli.exe` sibling for capture when present (the cli
/// frontend emits the "RenderReady" stdout token the resolve gate
/// waits on and boots straight into the game, no Qt shell). Non-
/// Eden kinds pass through untouched.
fn eden_cli_sibling(exe: &str, kind: Kind) -> PathBuf {
    let p = PathBuf::from(exe);
    if kind == Kind::Eden
        && p.file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.eq_ignore_ascii_case("eden.exe"))
    {
        if let Some(dir) = p.parent() {
            let cli = dir.join("eden-cli.exe");
            if cli.is_file() {
                return cli;
            }
        }
    }
    p
}

/// The inverse for the setup gear — `eden-cli.exe` has no settings
/// UI, so a cli-resolved launcher swaps to its `eden.exe` sibling.
fn eden_gui_sibling(exe: &str) -> PathBuf {
    let p = PathBuf::from(exe);
    let name = p
        .file_stem()
        .and_then(|f| f.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if name.contains("eden-cli") || name.contains("edencli") {
        if let Some(dir) = p.parent() {
            let gui = dir.join("eden.exe");
            if gui.is_file() {
                return gui;
            }
        }
    }
    p
}

/// EdenHandler.ResolveEdenConfigPath — `%APPDATA%/eden/config`.
/// Off Windows: a managed XDG_CONFIG_HOME under the data root; the
/// child's qt-config.ini lands at `<dir>/qt-config.ini` and the env
/// var keeps Eden (Qt) from touching `~/.config/eden` or dropping
/// files relative to the CWD.
#[cfg(windows)]
fn eden_config_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("eden")
        .join("config")
}

#[cfg(not(windows))]
fn eden_config_dir() -> PathBuf {
    eden_xdg_dir().join("eden")
}

#[cfg(not(windows))]
fn eden_xdg_dir() -> PathBuf {
    crate::settings::data_root()
        .map(|d| d.join("Eden").join("xdg"))
        .unwrap_or_else(|| PathBuf::from(".eden-xdg"))
}

fn eden_config_path() -> PathBuf {
    eden_config_dir().join("qt-config.ini")
}

/// EdenHandler.EnsureEdenConfigOverrides — the ini keys a hidden
/// launch needs: fullscreen off, the status bar off, and
/// pause_on_focus_loss off (a cloaked window never has focus — left
/// on, Eden would pause the game the moment it boots). Writes a
/// missing file, rewrites matching `key=value` lines in place, and
/// appends whatever's absent — the file keeps everything else.
fn ensure_eden_setup() {
    let path = eden_config_path();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // Off Windows the file lives in a managed XDG dir — seed it from
    // the user's real `~/.config/eden/qt-config.ini` (if any) so their
    // tuned settings carry over before we apply launch overrides.
    #[cfg(not(windows))]
    if !path.exists() {
        let home_cfg = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".config")
            .join("eden")
            .join("qt-config.ini");
        if home_cfg.exists() {
            let _ = std::fs::copy(&home_cfg, &path);
        }
    }
    // Fullscreen only off-Windows: the XWM snaps fullscreen windows to
    // the capture output and Qt hides menu/status chrome there, while
    // on Windows the window is cloaked and must stay windowed.
    #[cfg(windows)]
    let fs = "false";
    #[cfg(not(windows))]
    let fs = "true";
    let mut desired: Vec<(&str, &str)> = vec![
        ("fullscreen\\default", "false"),
        ("fullscreen", fs),
        ("showStatusBar\\default", "false"),
        ("showStatusBar", "false"),
        ("pause_on_focus_loss\\default", "false"),
        ("pause_on_focus_loss", "false"),
        ("firstStart\\default", "false"),
        ("firstStart", "false"),
        // The update prompt can't be dismissed over the input tunnel.
        ("check_for_updates\\default", "false"),
        ("check_for_updates", "false"),
    ];
    let Ok(existing) = std::fs::read_to_string(&path) else {
        let body: String = desired.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        let _ = std::fs::write(&path, body);
        return;
    };
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let mut updated = false;
    for line in &mut lines {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || !t.contains('=') {
            continue;
        }
        let key = t.splitn(2, '=').next().unwrap_or("").trim();
        if let Some(pos) = desired
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(key))
        {
            let (_, v) = desired.remove(pos);
            let new_line = format!("{key}={v}");
            if t != new_line {
                *line = new_line;
                updated = true;
            }
        }
    }
    if !desired.is_empty() {
        for (k, v) in desired {
            lines.push(format!("{k}={v}"));
        }
        updated = true;
    }
    if updated {
        let _ = std::fs::write(&path, lines.join("\n") + "\n");
    }
}

/// `<data>/Emulators/3DS` — the section dir shared by the managed
/// RetroArch install (`RetroArch/`) and the Libretro cores
/// (`Libretro/`).
fn three_ds_root() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("3DS"))
}

/// The section root a RetroArch launch belongs to — the managed
/// core path is `<root>/Libretro/cores/<id>/<lib>`, so its
/// `Libretro` ancestor's parent IS the section dir (3DS, N64).
/// User-configured core paths outside the managed tree fall back
/// to the 3DS section.
fn retroarch_root(core: Option<&Path>) -> Option<PathBuf> {
    core.and_then(|c| {
        c.ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == "Libretro"))
            .and_then(|a| a.parent().map(|p| p.to_path_buf()))
    })
    .or_else(three_ds_root)
}

/// RetroArchHandler's focus-override appendconfig — a hidden window
/// never has focus, so pause_nonactive stays off and game-focus
/// hotkey grabs are disabled. On Windows RA runs plain windowed (the
/// parked window is the capture surface); on Linux it fullscreens
/// onto the nested compositor's output. video_threaded is off so the
/// core's retro_run stays on the main thread — azahar's threaded
/// video path faulted inside amdvlk under capture. The system dir
/// follows the resolved frontend (`<launcher>/system` — Lacrima's
/// GetRetroArchSystemDirectories shape: BIOS/firmware is shared
/// across every section's cores); save/state dirs stay under the
/// section's Libretro tree so core data survives regardless of which
/// retroarch.exe the handler resolves to. Written under the AES data
/// dir (always writable — a user-installed RetroArch may sit in
/// Program Files).
fn retroarch_append_config(exe: &Path, core: Option<&Path>) -> Option<PathBuf> {
    let root = retroarch_root(core)?;
    let libretro = root.join("Libretro");
    let path = root.join("retroarch-aes.cfg");
    let mut lines = vec![
        "pause_nonactive = \"false\"".to_string(),
        "input_auto_game_focus = \"0\"".to_string(),
        // Windowed on Windows (the parked window is the capture
        // surface); fullscreen on Linux so RA's toplevel fills the
        // nested compositor's output.
        format!(
            "video_fullscreen = \"{}\"",
            if cfg!(windows) { "false" } else { "true" }
        ),
        "video_threaded = \"false\"".to_string(),
        format!(
            "system_directory = \"{}\"",
            exe.parent()
                .unwrap_or(&libretro)
                .join("system")
                .display()
        ),
        format!(
            "savefile_directory = \"{}\"",
            libretro.join("saves").display()
        ),
        format!(
            "savestate_directory = \"{}\"",
            libretro.join("states").display()
        ),
    ];
    // PPSSPP's bundled GLEW can't init under RA's glcore driver (core
    // profile) — `gl` gives it the compatibility context its G3D
    // backend needs. PSP-section launches only, Linux only (Windows
    // keeps glcore — D3D/GPU capture doesn't hit the GLEW path).
    if !cfg!(windows) && root.file_name().is_some_and(|n| n == "PSP") {
        lines.push("video_driver = \"gl\"".to_string());
    }
    let body = lines.join("\n") + "\n";
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::create_dir_all(&root).ok()?;
        std::fs::write(&path, body).ok()?;
    }
    // RA ignores a *_directory override that doesn't exist
    // ("savefile_directory is not a directory, ignoring") — the dirs
    // must exist BEFORE spawn or saves silently fall back to the
    // frontend's own dirs.
    let _ = std::fs::create_dir_all(exe.parent().unwrap_or(&root).join("system"));
    let _ = std::fs::create_dir_all(libretro.join("saves"));
    let _ = std::fs::create_dir_all(libretro.join("states"));
    Some(path)
}

/// RetroArchHandler.SaturnBiosFileNames — the Saturn IPL dumps
/// Beetle/Yabause-family cores load from the frontend system dir.
const SATURN_BIOS_FILES: [&str; 3] = ["saturn_bios.bin", "sega_101.bin", "mpr-17933.bin"];

/// RequiresSaturnBios — Lacrima gates on the core filename matching
/// saturn/yabause/yabasanshiro/beetle (empty selection counts).
fn saturn_core_needs_bios(core: &Path) -> bool {
    let name = core
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.is_empty()
        || name.contains("saturn")
        || name.contains("yabause")
        || name.contains("yabasanshiro")
        || name.contains("beetle")
}

/// TryGetSaturnBiosValidationError — the pre-launch firmware gate for
/// the Saturn handler. The effective system dir is the one our
/// appendconfig writes: `<frontend>/system` (the shared
/// `Emulators/RetroArch/system` for managed installs). Returns the
/// actionable multi-line message when no region's dump is there.
pub fn saturn_bios_error(exe: &Path, core: &Path) -> Option<String> {
    if !saturn_core_needs_bios(core) {
        return None;
    }
    let sys = exe.parent()?.join("system");
    if SATURN_BIOS_FILES.iter().any(|f| sys.join(f).is_file()) {
        return None;
    }
    Some(format!(
        "Sega Saturn BIOS files are missing.\n\n\
         Copy legally obtained BIOS dumps into this RetroArch system folder:\n\
         {}\n\n\
         Required files (at least one region):\n\
           - saturn_bios.bin\n  - sega_101.bin\n  - mpr-17933.bin\n\n\
         US: saturn_bios.bin | Japan: sega_101.bin | Europe: mpr-17933.bin",
        sys.display()
    ))
}

/// The `--verbose --log-file` target — retroarch-launch.log under
/// the section dir (RetroArchHandler's GetRetroArchLogFilePath
/// shape). The launch-failure path reads its last error line.
fn retroarch_log_path(core: Option<&Path>) -> Option<PathBuf> {
    let root = retroarch_root(core)?;
    let _ = std::fs::create_dir_all(&root);
    Some(root.join("retroarch-launch.log"))
}

/// EdenHandler's WaitForRenderReadyOutputAsync — eden-cli logs
/// "RenderReady" once the render surface exists; the resolve gate
/// reads the launch transcript incrementally so the token can skip
/// the remaining stability polls.
#[cfg(windows)]
fn eden_render_ready(transcript: Option<&Path>, pos: &mut u64) -> bool {
    let Some(tp) = transcript else {
        return false;
    };
    use std::io::{Read, Seek};
    let Ok(meta) = std::fs::metadata(tp) else {
        return false;
    };
    if meta.len() < *pos {
        *pos = 0;
    }
    if meta.len() == *pos {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(tp) else {
        return false;
    };
    let _ = f.seek(std::io::SeekFrom::Start(*pos));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    *pos += buf.len() as u64;
    String::from_utf8_lossy(&buf)
        .to_lowercase()
        .contains("renderready")
}

/// Set `key = value` inside `[section]` of an ini — add the section
/// when missing (TrySetDolphinIniValue/TrySetDolphinCoreIniValue).
fn set_ini_key(path: &Path, section: &str, key: &str, value: &str) {
    let header = format!("[{section}]");
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(c) => c.lines().map(str::to_string).collect(),
        Err(_) => vec![header.clone()],
    };
    let new_line = format!("{key} = {value}");
    let mut dirty = false;
    // Replace every in-section key line (Dolphin can emit dupes).
    let mut in_section = false;
    let mut found = false;
    for line in &mut lines {
        let t = line.trim_start();
        if t.starts_with('[') {
            in_section = t.eq_ignore_ascii_case(&header);
            continue;
        }
        if in_section
            && line.contains('=')
            && t.get(..key.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(key))
        {
            found = true;
            if t != new_line {
                *line = new_line.clone();
                dirty = true;
            }
        }
    }
    if !found {
        let mut at = lines
            .iter()
            .position(|l| l.trim().eq_ignore_ascii_case(&header))
            .map(|i| i + 1);
        if at.is_none() {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(header);
            at = Some(lines.len());
        }
        lines.insert(at.unwrap_or(lines.len()), new_line);
        dirty = true;
    }
    if dirty {
        let _ = std::fs::write(path, lines.join("\n") + "\n");
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
#[cfg(windows)]
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
    transcript: Option<PathBuf>,
) {
    let set_status = |s: Status| {
        session_log(&format!("pid={pid} status -> {s:?}"));
        status.lock().unwrap_or_else(|p| p.into_inner()).status = s;
    };

    let mut proc = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            pid,
        )
    }
    .unwrap_or_default();
    // Eden's launcher can hand off to a child process and exit
    // (ResolveRuntimeProcessAsync → TryResolveChildProcess) — `pid`
    // tracks the live descendant when that happens.
    let mut pid = pid;
    let mut eden_ready = false;
    let mut eden_pos = 0u64;

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
        // CaptureStartupDelayMs = 120/200 — the Qt emulators' early
        // windows settle fast.
        Kind::DuckStation => 120,
        Kind::Pcsx2 => 200,
        // Dolphin's CaptureStartupDelayMs — its Qt shell churns
        // through several windows before the render surface exists.
        Kind::Dolphin => 1800,
        // CemuHandler.CaptureStartupDelayMs = 250; Eden's is 250
        // too (EdenHandler.CaptureStartupDelayMs). RetroArch's is
        // 200 (RetroArchHandler.CaptureStartupDelayMs).
        Kind::Cemu | Kind::Eden => 250,
        Kind::RetroArch => 200,
        _ => 0,
    };
    let poll = if matches!(
        kind,
        Kind::Rpcs3
            | Kind::Shadps4
            | Kind::DuckStation
            | Kind::Pcsx2
            | Kind::Dolphin
            | Kind::Cemu
            | Kind::Eden
            | Kind::RetroArch
    ) {
        25
    } else {
        100
    };
    let mut waited = 0u64;
    while waited < delay {
        sweep_hide(pid, HWND::default(), &mut hidden, kind);
        if stop.load(Ordering::Relaxed)
            || (!alive(proc) && !(kind == Kind::Eden && eden_rehome(&mut proc, &mut pid)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(poll));
        waited += poll;
    }
    // WaitForCoreLinkerOutputAsync — shadPS4's stdout transcript
    // emits `[Core.Linker]` once the game's modules are linked; the
    // SDL render window before that is a loader surface, not the
    // game. Up to 45s, sweeping every 50ms so the early window stays
    // cloaked the whole wait.
    if kind == Kind::Shadps4 {
        if let Some(tp) = &transcript {
            let deadline = Instant::now() + Duration::from_secs(45);
            let mut pos = 0u64;
            let mut hit = false;
            while Instant::now() < deadline && !hit {
                if stop.load(Ordering::Relaxed) || !alive(proc) {
                    break;
                }
                sweep_hide(pid, HWND::default(), &mut hidden, kind);
                if let Ok(meta) = std::fs::metadata(tp) {
                    if meta.len() < pos {
                        pos = 0;
                    }
                    if meta.len() > pos {
                        use std::io::{Read, Seek};
                        if let Ok(mut f) = std::fs::File::open(tp) {
                            let _ = f.seek(std::io::SeekFrom::Start(pos));
                            let mut buf = Vec::new();
                            let _ = f.read_to_end(&mut buf);
                            pos += buf.len() as u64;
                            let text = String::from_utf8_lossy(&buf);
                            hit = text
                                .lines()
                                .any(|l| l.to_lowercase().contains("core.linker"));
                        }
                    }
                }
                if !hit {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    // CemuHandler.WaitForRenderReadyLogAsync — Cemu's shell windows
    // appear long before the render surface, so resolution waits on
    // the emulator's own log.txt for "------- Run title -------"
    // (90s cap, 100ms polls, cloaking so the shell stays hidden the
    // whole wait). transcript carries the exe dir — portable mode
    // writes portable/log.txt, a 1.x/non-portable exe writes
    // log.txt or %APPDATA%/Cemu/log.txt (ResolveCemuLogFilePath).
    if kind == Kind::Cemu {
        if let Some(dir) = &transcript {
            let logs = cemu_log_candidates(dir);
            let deadline = Instant::now() + Duration::from_secs(90);
            while Instant::now() < deadline {
                if stop.load(Ordering::Relaxed) || !alive(proc) {
                    break;
                }
                sweep_hide(pid, HWND::default(), &mut hidden, kind);
                let ready = logs.iter().any(|p| {
                    std::fs::read_to_string(p)
                        .map(|t| t.to_lowercase().contains("run title"))
                        .unwrap_or(false)
                });
                if ready {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let mut hwnd = HWND::default();
    let mut stable_hwnd = HWND::default();
    let mut stable_count = 0u32;
    // RPCS3's resolve state: the window's last stable client size and
    // whether its 16:9 capture geometry has been applied yet.
    let mut rpcs3_geom = false;
    let mut rpcs3_size = (0i32, 0i32);
    // Dolphin's/Cemu's gates count same-hwnd-AND-size polls (their
    // Qt windows resize through boot) — tracked separately from
    // rpcs3's.
    let mut stable_size = (0i32, 0i32);
    // 25ms polls for the Qt emulators keep the cloak gap sub-frame;
    // other kinds sweep at 50ms (window flash isn't an issue once
    // captured). Dolphin joins — its Qt shell otherwise flashes the
    // launcher window before capture takes over.
    let poll = if matches!(
        kind,
        Kind::Rpcs3
            | Kind::Shadps4
            | Kind::DuckStation
            | Kind::Pcsx2
            | Kind::Dolphin
            | Kind::Cemu
            | Kind::Eden
    ) {
        25
    } else {
        50
    };
    // Dolphin/Cemu get Lacrima's 400×50ms resolve budget — their
    // render windows appear late after a long Qt boot (Cemu's
    // resolve runs after the log-ready gate anyway).
    let attempts = if matches!(kind, Kind::Dolphin | Kind::Cemu) {
        400
    } else {
        240
    };
    for _ in 0..attempts {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if kind == Kind::Xenia {
            dismiss_warning_dialogs(pid);
        }
        if !alive(proc) && !(kind == Kind::Eden && eden_rehome(&mut proc, &mut pid)) {
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
                    hidden.entry(hwnd.0 as usize).or_insert_with(|| {
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
            if kind == Kind::Shadps4 {
                // CaptureWindowAspectRatio = 16:9 applied once on
                // resolve — no stability gate (Lacrima resizes the
                // target as soon as it's found).
                hidden.entry(hwnd.0 as usize).or_insert_with(|| {
                    let s = snapshot_window(hwnd);
                    set_cloaked(hwnd, true);
                    s
                });
                prepare_window_for_capture(hwnd);
                apply_aspect_ratio(hwnd, 16.0 / 9.0);
            }
            if matches!(kind, Kind::DuckStation | Kind::Pcsx2) {
                // ResolveCaptureTargetAsync: the candidate must be
                // ≥640×360 client AND the same hwnd across 6
                // consecutive polls (stableAttemptsBeforeAssign) —
                // the Qt UI settles before the game surface is
                // stable.
                let (w, h) = client_size(hwnd).unwrap_or((0, 0));
                if w < 640 || h < 360 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                stable_count = if hwnd == stable_hwnd {
                    stable_count + 1
                } else {
                    stable_hwnd = hwnd;
                    1
                };
                if stable_count < 6 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                hidden.entry(hwnd.0 as usize).or_insert_with(|| {
                    let s = snapshot_window(hwnd);
                    set_cloaked(hwnd, true);
                    s
                });
                prepare_window_for_capture(hwnd);
                // CaptureWindowAspectRatio: 4:3 for PSX; PCSX2 has
                // no override — the emulator's own aspect stays.
                if kind == Kind::DuckStation {
                    apply_aspect_ratio(hwnd, 4.0 / 3.0);
                }
            }
            if kind == Kind::Dolphin {
                // IsStableCaptureCandidate + ResolveCaptureTargetAsync:
                // captioned, not minimized, ≥640×360 client — then the
                // same hwnd at the same client size for 8 consecutive
                // polls (Dolphin's Qt windows resize through boot).
                let (w, h) = client_size(hwnd).unwrap_or((0, 0));
                if w < 640 || h < 360 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
                if (style & WS_CAPTION.0 as i64) != WS_CAPTION.0 as i64 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                if unsafe { IsIconic(hwnd) }.as_bool() {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                stable_count = if hwnd == stable_hwnd && (w, h) == stable_size {
                    stable_count + 1
                } else {
                    stable_hwnd = hwnd;
                    stable_size = (w, h);
                    1
                };
                if stable_count < 8 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                // ApplyCaptureGeometryOnce: strip decorations, force
                // the 16:9 capture aspect (base default — Dolphin has
                // no override), then a short settle before attach.
                hidden.entry(hwnd.0 as usize).or_insert_with(|| {
                    let s = snapshot_window(hwnd);
                    set_cloaked(hwnd, true);
                    s
                });
                prepare_window_for_capture(hwnd);
                apply_aspect_ratio(hwnd, 16.0 / 9.0);
                std::thread::sleep(Duration::from_millis(120));
            }
            if kind == Kind::Cemu {
                // CemuHandler.ResolveCaptureTargetAsync +
                // IsStableCaptureCandidate: not minimized, ≥640×360
                // client, then the same hwnd at the SAME size for 8
                // consecutive polls (its Qt render surface resizes
                // through boot). No caption requirement — the title
                // filters already did that work.
                let (w, h) = client_size(hwnd).unwrap_or((0, 0));
                if w < 640 || h < 360 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                if unsafe { IsIconic(hwnd) }.as_bool() {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                stable_count = if hwnd == stable_hwnd && (w, h) == stable_size {
                    stable_count + 1
                } else {
                    stable_hwnd = hwnd;
                    stable_size = (w, h);
                    1
                };
                if stable_count < 8 {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                // ApplyCaptureGeometryOnce: strip decorations + the
                // wx menu bar + 16:9 capture aspect, then Lacrima's
                // 150ms settle. The one-shot menu strip is safe HERE
                // — the window's title/FPS updates every frame, so
                // its UI thread is pumping (unlike mid-sweep during
                // boot, where SetMenu can block for good).
                hidden.entry(hwnd.0 as usize).or_insert_with(|| {
                    let s = snapshot_window(hwnd);
                    set_cloaked(hwnd, true);
                    s
                });
                prepare_window_for_capture(hwnd);
                strip_menu(hwnd);
                apply_aspect_ratio(hwnd, 16.0 / 9.0);
                std::thread::sleep(Duration::from_millis(150));
            }
            if kind == Kind::Eden {
                // EdenHandler.ResolveCaptureTargetAsync: the same
                // hwnd across 2 consecutive polls
                // (stableAttemptsBeforeAssign) — or immediate assign
                // once the cli transcript emits "RenderReady".
                stable_count = if hwnd == stable_hwnd {
                    stable_count + 1
                } else {
                    stable_hwnd = hwnd;
                    1
                };
                if !eden_ready {
                    eden_ready = eden_render_ready(transcript.as_deref(), &mut eden_pos);
                }
                if stable_count < 2 && !eden_ready {
                    std::thread::sleep(Duration::from_millis(poll));
                    continue;
                }
                // PrepareProcess/WindowForCapture are no-ops for
                // Eden (the sweep must not touch its windows), but
                // the capture host still strips the resolved
                // target's decorations at session start —
                // EnsureSessionCore's `HasWindowCaption →
                // RemoveWindowDecorations`, which runs for every
                // handler. Without it WGC frames include the
                // title bar. Post-resolve the window's UI thread is
                // pumping, so the SetMenu/SetWindowPos calls inside
                // return promptly.
                hidden.entry(hwnd.0 as usize).or_insert_with(|| {
                    let s = snapshot_window(hwnd);
                    set_cloaked(hwnd, true);
                    s
                });
                prepare_window_for_capture(hwnd);
            }
            hwnd_out.store(hwnd.0 as usize, Ordering::Relaxed);
            break;
        }
        stable_count = 0;
        stable_hwnd = HWND::default();
        std::thread::sleep(Duration::from_millis(poll));
    }

    if hwnd.is_invalid() {
        let failure = if stop.load(Ordering::Relaxed) {
            Status::Exited
        } else if !alive(proc) {
            if kind == Kind::RetroArch {
                let reason = transcript
                    .as_ref()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|s| retroarch_error_notice(&s))
                    .unwrap_or_else(|| "RetroArch exited before creating its window".into());
                Status::Failed(reason)
            } else {
                Status::Exited
            }
        } else {
            Status::Failed("no render window found".into())
        };
        set_status(failure);
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
    let saved = hidden.remove(&(hwnd.0 as usize)).unwrap_or_else(|| {
        // PrepareWindowForCapture is a no-op for Eden — an
        // unswept target just gets state-snapshotted, never
        // restyled mid-boot (same reason Lacrima no-ops it).
        if matches!(kind, Kind::Eden | Kind::RetroArch) {
            snapshot_window(hwnd)
        } else {
            prepare_window_for_capture(hwnd)
        }
    });
    if matches!(kind, Kind::Dolphin | Kind::Cemu | Kind::Eden) {
        // hide_window's WS_EX_LAYERED + 0-alpha can stall Vulkan/
        // OpenGL presentation (why Lacrima no-ops Dolphin's and
        // Cemu's window prep). Park offscreen under the cloak instead
        // — same offscreen corner, toolwindow'd out of the taskbar,
        // no layering.
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            let _ = SetWindowPos(
                hwnd,
                None,
                vx + (vw - 100).max(0),
                vy + (vh - 100).max(0),
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as i64;
            let new_ex = (ex | WS_EX_TOOLWINDOW.0 as i64) & !(WS_EX_APPWINDOW.0 as i64);
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
        }
        set_cloaked(hwnd, true);
    } else if kind != Kind::RetroArch {
        hide_window(hwnd);
    }
    // Cemu: never send into the wx UI thread (SetMenu blocks while
    // it's busy) — the menubar goes away under RemoveWindow-
    // Decorations + the game render anyway. RetroArch has no HMENU;
    // sending SetMenu/DrawMenuBar into its busy window thread mid-
    // boot is the same blocking hazard.
    if !matches!(kind, Kind::Cemu | Kind::RetroArch) {
        strip_menu(hwnd);
    }
    // Catch anything that appeared since the last poll.
    sweep_hide(pid, hwnd, &mut hidden, kind);

    if kind == Kind::RetroArch {
        // The sweep has had the whole process cloaked since spawn —
        // move the target to the corner BEFORE uncloaking so nothing
        // ever flashes on-screen. Lacrima's WgcCaptureControl runs
        // Win32API.RestoreWindow (ShowWindow SW_RESTORE) right before
        // StartCapture: RA creates its window WITHOUT WS_VISIBLE and
        // shows it a beat later — attach during the hidden window
        // fails CreateForWindow with E_INVALIDARG and leaves the game
        // running uncaptured. SW_RESTORE forces it visible.
        park_retroarch_window(hwnd);
        set_cloaked(hwnd, false);
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    // 3. Attach WGC — frames land in the shared slots (CPU mirror +
    //    zero-copy GPU ring).
    let mut capture = wgc::Capture::start(
        hwnd,
        frames.clone(),
        gpu.clone(),
        render.clone(),
        low_latency,
    );
    set_status(Status::Capturing);
    if kind == Kind::RetroArch {
        // RetroArchHandler: Prepare*ForCapture are no-ops so the
        // Vulkan window renders until the capture exists; the window
        // handler then runs Win32API.MoveAway — a bare SetWindowPos to
        // the virtual-screen corner and NOTHING else.
        undecorate_retroarch_window(hwnd);
        park_retroarch_window(hwnd);
    }

    // 4. Monitor: stop request, process exit, or capture failure.
    //    The sweep keeps parking late windows (dialogs, consoles) the
    //    emulator creates after the target resolved.
    let mut capture_err: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed)
            || (!alive(proc) && !(kind == Kind::Eden && eden_rehome(&mut proc, &mut pid)))
        {
            break;
        }
        sweep_hide(pid, hwnd, &mut hidden, kind);
        if kind == Kind::RetroArch {
            retroarch_watchdog(
                pid,
                &mut hwnd,
                &mut capture,
                &frames,
                &gpu,
                &render,
                low_latency,
                &hwnd_out,
            );
        }
        // The capture target isn't swept — strip its menu too so a
        // late-attached menu bar can't leak into the captured frame.
        // (Skipped for Cemu and RetroArch — SetMenu/DrawMenuBar can
        // block on their busy window threads.)
        if !matches!(kind, Kind::Cemu | Kind::RetroArch) {
            strip_menu(hwnd);
        }
        if let Some(err) = frames.lock().unwrap_or_else(|p| p.into_inner()).err.clone() {
            capture_err = Some(err);
            break;
        }
        std::thread::sleep(Duration::from_millis(120));
    }

    drop(capture);

    if let Some(err) = capture_err {
        // Capture died but the game may still run — put the windows
        // back so the user can interact with/close it. RetroArch is
        // the exception: its window was only snapshotted, never
        // mutated — writing the saved style back can clobber the
        // WS_VISIBLE RA sets itself after our snapshot (leaving the
        // window permanently hidden and uncapturable).
        if kind != Kind::RetroArch {
            restore_window(hwnd, &saved);
        } else {
            // RA's windows were cloaked since spawn — uncloak them
            // all so the user can see and close the emulator.
            for w in process_windows_of(&win_pids(pid, kind), true) {
                set_cloaked(w, false);
            }
        }
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
        let grace = match kind {
            Kind::Rpcs3 => {
                send_ctrl_s(hwnd);
                // With --no-gui a landed Ctrl+S stops the game and
                // exits within ~a second; if it didn't land, don't
                // sit in the full 5s wait — cut to TerminateProcess.
                2500
            }
            // Lacrima's ForceCloseTrackedEmulatorProcess kills
            // shadPS4, DuckStation, PCSX2 and Dolphin outright — a
            // WM_CLOSE pops their confirm-exit dialog on a hidden
            // window.
            Kind::Shadps4 | Kind::DuckStation | Kind::Pcsx2 | Kind::Dolphin => 0,
            _ => {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                2500
            }
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
    // Eden may have rehomed the tracked pid to a child — kill any
    // surviving descendants too (the tree is the emulator).
    if kind == Kind::Eden {
        for dp in descendant_pids(pid) {
            if let Ok(h) = unsafe { OpenProcess(PROCESS_TERMINATE, false, dp) } {
                unsafe {
                    let _ = TerminateProcess(h, 0);
                    let _ = CloseHandle(h);
                }
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
#[cfg(windows)]
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

#[cfg(windows)]
struct EnumCtx {
    pids: Vec<u32>,
    include_hidden: bool,
    out: Vec<HWND>,
}

#[cfg(windows)]
unsafe extern "system" fn enum_top(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if !ctx.include_hidden && !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut wpid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut wpid));
    if ctx.pids.contains(&wpid) {
        ctx.out.push(hwnd);
    }
    BOOL(1)
}

#[cfg(windows)]
fn process_windows(pid: u32, include_hidden: bool) -> Vec<HWND> {
    process_windows_of(&[pid], include_hidden)
}

#[cfg(windows)]
fn process_windows_of(pids: &[u32], include_hidden: bool) -> Vec<HWND> {
    let mut ctx = EnumCtx {
        pids: pids.to_vec(),
        include_hidden,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(enum_top), LPARAM(&mut ctx as *mut EnumCtx as isize));
    }
    ctx.out
}

/// The pids whose windows belong to this launch — `pid` alone for
/// every emulator except Eden, which can render on a child process
/// (ResolveRuntimeProcessAsync walks descendants for the same
/// reason): pid + every live descendant.
#[cfg(windows)]
fn win_pids(pid: u32, kind: Kind) -> Vec<u32> {
    if kind != Kind::Eden {
        return vec![pid];
    }
    let mut v = vec![pid];
    v.extend(descendant_pids(pid));
    v
}

/// Lacrima's EnumerateDescendantProcesses — a Toolhelp32 snapshot
/// BFS over parent-pid edges (children of children included).
#[cfg(windows)]
fn descendant_pids(root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut all = Vec::new();
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                all.push((e.th32ProcessID, e.th32ParentProcessID));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let mut queue = vec![root];
        let mut seen = std::collections::HashSet::from([root]);
        while let Some(p) = queue.pop() {
            for &(cpid, ppid) in &all {
                if ppid == p && seen.insert(cpid) {
                    out.push(cpid);
                    queue.push(cpid);
                }
            }
        }
    }
    out
}

/// EdenHandler.ResolveRuntimeProcessAsync → TryResolveChildProcess —
/// when the spawned eden.exe/eden-cli.exe hands off to a child and
/// exits, re-home the tracked handle to a live descendant so window
/// enumeration, the alive check, and teardown keep working.
#[cfg(windows)]
fn eden_rehome(proc: &mut HANDLE, pid: &mut u32) -> bool {
    for dp in descendant_pids(*pid) {
        let Ok(h) = (unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                dp,
            )
        }) else {
            continue;
        };
        if unsafe { WaitForSingleObject(h, 0) } == WAIT_TIMEOUT {
            if !proc.is_invalid() {
                unsafe {
                    let _ = CloseHandle(*proc);
                }
            }
            *proc = h;
            *pid = dp;
            return true;
        }
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    false
}

#[cfg(windows)]
struct ChildCtx {
    pid: u32,
    kind: Kind,
    /// shadPS4's FindBestRenderChildWindow only scores DIRECT
    /// children of the resolved top-level.
    parent: HWND,
    best: HWND,
    best_score: i64,
}
#[cfg(windows)]

unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut ChildCtx);
    let score = match ctx.kind {
        Kind::Rpcs3 => score_rpcs3_child(hwnd, ctx.pid),
        Kind::Shadps4 => score_shadps4_child(hwnd, ctx.pid, ctx.parent),
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
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut r = RECT::default();
    unsafe { GetClientRect(hwnd, &mut r) }.ok()?;
    Some((r.right - r.left, r.bottom - r.top))
}

/// IsRpcs3DialogOrOverlayWindow — boot progress/trophy/shader
/// dialogs that must never be picked as the render surface.
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
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
    if !looks_primary && !t.is_empty() && (class.contains("qt") || class.contains("qwindow")) {
        return true;
    }
    !looks_primary && w >= 960 && h >= 540 && !t.is_empty()
}

/// ScoreRpcs3RenderChildWindow — gs_frame can also hang off the main
/// window as a Qt child; same gates as the top-level path with
/// render-class bonuses.
#[cfg(windows)]
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

/// IsLikelyShadPs4RenderWindow — the game surface is a top-level SDL
/// window; the launcher/settings chrome and child surfaces are
/// filtered out, then a direct render-class child of the top-level
/// can win over it (FindBestRenderChildWindow).
#[cfg(windows)]
fn is_likely_shadps4_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let title = window_title(hwnd);
    let t = title.trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    // WGC on child HWNDs produces edge corruption — reject them.
    if (style & WS_CHILD.0 as i64) != 0 {
        return false;
    }
    let parent = unsafe { GetParent(hwnd) }.unwrap_or_default();
    if !parent.is_invalid() {
        return false;
    }
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let mut looks_primary = hwnd == main;
    for bad in [
        "qtlauncher",
        "settings",
        "controller",
        "about",
        "compatibility",
        "version manager",
        "game install",
        "launcher",
    ] {
        if t.contains(bad) {
            return false;
        }
    }
    if class.contains("ime") || class.contains("tooltip") || class.contains("observer") {
        return false;
    }
    if class.contains("sdl") || class.contains("vulkan") || class.contains("render") {
        return true;
    }
    if t.starts_with("fps:") || t.contains("vulkan") || t.contains("opengl") {
        return true;
    }
    if !t.is_empty() {
        if t.contains("shadps4") || t.contains("qt") {
            looks_primary = true;
        } else if (hwnd == main || !looks_primary) && t.chars().count() >= 2 {
            return true;
        }
    }
    if class.contains("qt") && looks_primary {
        return false;
    }
    !looks_primary && (!has_caption || !t.is_empty())
}

/// ScoreRenderChildWindow — a direct render-class child of the
/// resolved top-level (SDL/Vulkan surface) wins the capture.
#[cfg(windows)]
fn score_shadps4_child(hwnd: HWND, pid: u32, parent: HWND) -> i64 {
    if hwnd.is_invalid() {
        return i64::MIN;
    }
    if !parent.is_invalid() && unsafe { GetParent(hwnd) }.unwrap_or_default() != parent {
        return i64::MIN;
    }
    let mut wpid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut wpid));
    }
    if wpid != pid {
        return i64::MIN;
    }
    let Some((w, h)) = client_size(hwnd) else {
        return i64::MIN;
    };
    if w < 320 || h < 180 {
        return i64::MIN;
    }
    let class = class_name(hwnd).to_lowercase();
    if !(class.contains("sdl") || class.contains("vulkan") || class.contains("render")) {
        return i64::MIN;
    }
    w as i64 * h as i64
}

/// IsLikelyDuckStationRenderWindow — the game surface is a Qt
/// top-level; the duckstation chrome (settings/input/cheats/…) and
/// IME/tooltip windows are rejected. It can also render in the main
/// window with an EMPTY title while a game loads.
#[cfg(windows)]
fn is_likely_duckstation_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let t = window_title(hwnd).trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    if t.contains("_q_titlebar")
        || t.contains("msctfime ui")
        || t.contains("default ime")
        || class.contains("screenchangeobserver")
        || class.contains("themechangeobserver")
        || class.contains("ime")
    {
        return false;
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let mut looks_primary = hwnd == main;
    if looks_primary && t.is_empty() && class.contains("qt") && !has_caption {
        return true;
    }
    if !t.is_empty() {
        let ui = t == "duckstation"
            || [
                "settings",
                "audio",
                "video",
                "input",
                "controller",
                "memory card",
                "cheat",
                "achievement",
                "tools",
                "view",
                "help",
                "shader",
                "about",
            ]
            .iter()
            .any(|w| t.contains(w));
        if ui {
            looks_primary = true;
        } else if t.chars().count() >= 2 {
            return true;
        }
    }
    if !class.is_empty() && class.contains("qt") {
        if !looks_primary && !t.contains("duckstation") {
            return true;
        }
        if !looks_primary && !has_caption {
            return true;
        }
    }
    !looks_primary && (!has_caption || !t.is_empty())
}

/// IsLikelyPcsx2RenderWindow — the game surface is a Qt top-level;
/// pcsx2 chrome (settings/graphics/controller/…) and IME/tooltip
/// windows are rejected. Qt class names like "Qt6110QWindowIcon" on
/// a non-primary window count as candidates.
#[cfg(windows)]
fn is_likely_pcsx2_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let t = window_title(hwnd).trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    if t.contains("_q_titlebar")
        || t.contains("msctfime ui")
        || t.contains("default ime")
        || class.contains("screenchangeobserver")
        || class.contains("themechangeobserver")
        || class.contains("ime")
    {
        return false;
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let has_thick = (style & WS_THICKFRAME.0 as i64) == WS_THICKFRAME.0 as i64;
    let mut looks_primary = hwnd == main;
    if !t.is_empty() {
        let ui = [
            "pcsx2",
            "settings",
            "graphics",
            "audio",
            "controller",
            "memory card",
            "cheat",
            "tools",
            "about",
            "emulation settings",
            "game properties",
        ]
        .iter()
        .any(|w| t.contains(w));
        if ui {
            looks_primary = true;
        } else if t.chars().count() >= 2 {
            return true;
        }
    }
    if !class.is_empty() {
        if class.contains("pcsx2") && has_caption && has_thick {
            looks_primary = true;
        }
        if !looks_primary && class.contains("qt") && !t.contains("pcsx2") {
            return true;
        }
    }
    !looks_primary && (!has_caption || !t.is_empty())
}

/// IsLikelyDolphinRenderWindow — the game surface carries the
/// "Dolphin <ver> | <game>" title pattern; Qt/wx-class windows and
/// captioned dolphin-class windows count as candidates, while the
/// config/audio/controller dialogs, IME helpers and the primary UI
/// shell are rejected.
#[cfg(windows)]
fn is_likely_dolphin_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let t = window_title(hwnd).trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let has_thick = (style & WS_THICKFRAME.0 as i64) == WS_THICKFRAME.0 as i64;
    let looks_primary = hwnd == main;
    // Helper/system windows in the process tree.
    if t.contains("diemwin")
        || t.contains("temp window")
        || t.contains("msctfime ui")
        || t.contains("default ime")
        || class.starts_with("temp_d3d_window")
        || class.contains("diemwin")
        || class.contains("ime")
        || class.contains("screenchangeobserver")
        || class.contains("themechangeobserver")
    {
        return false;
    }
    // "Dolphin 2503 | Metroid Prime" — the game's titlebar.
    if t.contains("dolphin") && t.contains(" | ") {
        return true;
    }
    let ui_title = [
        "configuration",
        "graphics",
        "audio",
        "controller",
        "hotkey",
        "tools",
        "about",
        "fifo",
        "shader",
        "memory card",
        "netplay",
    ]
    .iter()
    .any(|w| t.contains(w));
    // The bare launcher window, e.g. "Dolphin 2503".
    if t.starts_with("dolphin ") && !t.contains(" | ") {
        return false;
    }
    if t.contains("dolphin") && !ui_title {
        return true;
    }
    if !ui_title && t.chars().count() >= 3 {
        if class.contains("qt") || class.contains("wx") || class.contains("dolphin") {
            return true;
        }
    }
    if t.is_empty()
        && (class.contains("qt") || class.contains("wx") || class.contains("dolphin"))
        && !looks_primary
    {
        return true;
    }
    if class.contains("dolphin") && has_caption && has_thick {
        return !looks_primary;
    }
    false
}

/// IsLikelyCemuRenderWindow — Cemu keeps its shell (title list,
/// settings, getting-started) alive alongside the render surface;
/// reject the shell/dialog titles, accept "fps:/loading/compiling"
/// boot titles and any well-formed game title, and fall back to a
/// Qt-class non-main window with a real client size.
#[cfg(windows)]
fn is_likely_cemu_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let title = window_title(hwnd).trim().to_string();
    let t = title.to_lowercase();
    // Helper/system windows in the process tree (Cemu spawns a
    // hidden temp_d3d_window + IME/tooltips surface).
    if t.contains("temp window") || t.contains("msctfime ui") || t.contains("default ime") {
        return false;
    }
    // IsCemuShellWindow.
    if t.is_empty()
        || t == "cemu"
        || t.contains("title list")
        || t.contains("getting started")
        || t.contains("graphic pack")
        || t.contains("input settings")
        || t.contains("general settings")
    {
        return false;
    }
    if t.contains("settings")
        || t.contains("about")
        || t.contains("profile")
        || t.contains("update")
        || t.contains("options")
        || t.contains("cemu hook")
    {
        return false;
    }
    if t.contains("fps:") || t.contains("loading") || t.contains("compiling") {
        return true;
    }
    if title.chars().count() > 5 {
        return true;
    }
    let class = class_name(hwnd).to_lowercase();
    if (class.contains("qwindow") || class.contains("qt6")) && hwnd != main {
        let (w, h) = client_size(hwnd).unwrap_or((0, 0));
        if w >= 480 && h >= 270 {
            return true;
        }
    }
    hwnd == main && !t.is_empty() && t != "cemu"
}

/// EdenHandler's IsLikelyEdenRenderWindow — settings/tool dialogs
/// out (their titles carry the dialog kind), then SDL/GLFW/Qt/Eden
/// classes, an "eden"-titled window, the main window, or a window
/// missing caption/thick-frame chrome (a bare render surface).
#[cfg(windows)]
fn is_likely_eden_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let title = window_title(hwnd).trim().to_lowercase();
    let class = class_name(hwnd).to_lowercase();
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as i64;
    let has_caption = (style & WS_CAPTION.0 as i64) == WS_CAPTION.0 as i64;
    let has_thick = (style & WS_THICKFRAME.0 as i64) == WS_THICKFRAME.0 as i64;
    if !title.is_empty()
        && [
            "settings",
            "audio",
            "video",
            "input",
            "controller",
            "cheat",
            "shader",
            "about",
        ]
        .iter()
        .any(|t| title.contains(t))
    {
        return false;
    }
    if !class.is_empty() {
        if class.contains("sdl") || class.contains("glfw") {
            return true;
        }
        // Recent builds can render in the Qt main window itself.
        if class.contains("qt") || class.contains("eden") {
            return true;
        }
    }
    if title.contains("eden") {
        return true;
    }
    if hwnd == main {
        return true;
    }
    !has_caption || !has_thick
}

/// RetroArch renders into its own win32 window — class "RetroArch",
/// title "RetroArch <version> — <core>" once a core is running.
/// RetroArchHandler.IsLikelyRetroArchRenderWindow — menu-ish titles
/// lose, then frontend/emulator title and class hints, then the
/// process-main-window fallback. No size floor: RA's window can be
/// small before the core's first SET_GEOMETRY resize.
#[cfg(windows)]
fn is_likely_retroarch_window(hwnd: HWND, main: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    // Aux windows first — they report the OWNER's pid (console
    // windows do on Win11), so Panda3DS's "Log Console" would win
    // the `hwnd == main` fallback and we'd capture the console
    // instead of the game.
    let class = class_name(hwnd).to_lowercase();
    if class.contains("console")
        || class.contains("ime")
        || class.starts_with("temp_d3d_window")
        || class == "diemwin"
    {
        return false;
    }
    let title = window_title(hwnd);
    let t = title.trim().to_lowercase();
    if !t.is_empty() {
        for bad in [
            "menu",
            "settings",
            "audio",
            "video",
            "input",
            "controller",
            "cheat",
            "shader",
            "playlist",
            "load content",
            "options",
            "config",
        ] {
            if t.contains(bad) {
                return false;
            }
        }
        if t.contains("retroarch") || t.contains("citra") || t.contains("3ds") {
            return true;
        }
    }
    if !class.is_empty()
        && (class.contains("retroarch")
            || class.contains("sdl")
            || class.contains("glfw")
            || class.contains("azahar")
            || class.contains("citra"))
    {
        return true;
    }
    hwnd == main
}

/// IsLikelyXeniaRenderWindow — heuristic for top-level candidates
/// when no dedicated render child exists.
#[cfg(windows)]
fn is_likely_render_window(hwnd: HWND, main: HWND, kind: Kind) -> bool {
    match kind {
        Kind::Flycast => return is_likely_flycast_window(hwnd, main),
        Kind::Xemu => return is_likely_xemu_window(hwnd, main),
        Kind::Rpcs3 => return is_likely_rpcs3_window(hwnd, main),
        Kind::Shadps4 => return is_likely_shadps4_window(hwnd, main),
        Kind::DuckStation => return is_likely_duckstation_window(hwnd, main),
        Kind::Pcsx2 => return is_likely_pcsx2_window(hwnd, main),
        Kind::Dolphin => return is_likely_dolphin_window(hwnd, main),
        Kind::Cemu => return is_likely_cemu_window(hwnd, main),
        Kind::Eden => return is_likely_eden_window(hwnd, main),
        Kind::RetroArch => return is_likely_retroarch_window(hwnd, main),
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
            "console",
            "debug",
            "log",
            "settings",
            "controller",
            "tools",
            "about",
            "help",
            "warning",
            "error",
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
#[cfg(windows)]
fn score_top_level(hwnd: HWND, pids: &[u32], main: HWND, kind: Kind) -> i64 {
    if hwnd.is_invalid() {
        return i64::MIN;
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let mut wpid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut wpid));
    }
    if !pids.contains(&wpid) {
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
    // RetroArch has no size floor — its predicate accepts the
    // process's main window at any size.
    if !matches!(kind, Kind::Xenia | Kind::RetroArch) && (w < 640 || h < 360) {
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
#[cfg(windows)]
fn resolve_capture_target(pid: u32, kind: Kind) -> HWND {
    let pids = win_pids(pid, kind);
    let tops = process_windows_of(&pids, true);
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
    // shadPS4 resolves differently: the top-level wins first, then a
    // DIRECT render-class child of that window overrides it
    // (FindBestRenderChildWindow — GetParent(child) == top).
    if kind == Kind::Shadps4 {
        let top = tops
            .iter()
            .copied()
            .map(|h| (h, score_top_level(h, &pids, main, kind)))
            .filter(|(_, s)| *s > i64::MIN)
            .max_by_key(|(_, s)| *s)
            .map(|(h, _)| h)
            .unwrap_or_default();
        if !top.is_invalid() {
            let mut ctx = ChildCtx {
                pid,
                kind,
                parent: top,
                best: HWND::default(),
                best_score: i64::MIN,
            };
            unsafe {
                let _ = EnumChildWindows(
                    Some(top),
                    Some(enum_child),
                    LPARAM(&mut ctx as *mut ChildCtx as isize),
                );
            }
            if ctx.best_score > i64::MIN && !ctx.best.is_invalid() {
                return ctx.best;
            }
            return top;
        }
        return HWND::default();
    }

    if matches!(kind, Kind::Xenia | Kind::Rpcs3) {
        let mut ctx = ChildCtx {
            pid,
            kind,
            parent: HWND::default(),
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
        .map(|h| (h, score_top_level(h, &pids, main, kind)))
        .filter(|(_, s)| *s > i64::MIN)
        .max_by_key(|(_, s)| *s)
        .map(|(h, _)| h)
        .unwrap_or_default()
}

/// TryDismissXeniaWarningDialogs — click OK on "Xenia Warning/Error/
/// Help" dialogs (or WM_CLOSE them) so boot isn't blocked by a modal.
#[cfg(windows)]
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

#[cfg(windows)]
struct ClickCtx {
    text: String,
    clicked: bool,
}

#[cfg(windows)]
unsafe extern "system" fn enum_button(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut ClickCtx);
    if ctx.clicked {
        return BOOL(0);
    }
    if !class_name(hwnd).eq_ignore_ascii_case("button") {
        return BOOL(1);
    }
    if !window_title(hwnd)
        .trim()
        .eq_ignore_ascii_case(ctx.text.trim_start_matches('&'))
    {
        return BOOL(1);
    }
    SendMessageW(hwnd, BM_CLICK, Some(WPARAM(0)), Some(LPARAM(0)));
    ctx.clicked = true;
    BOOL(0)
}

#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
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
    let remove =
        (WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as i64;
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
                    SWP_NOMOVE
                        | SWP_NOZORDER
                        | SWP_NOACTIVATE
                        | SWP_FRAMECHANGED
                        | SWP_NOOWNERZORDER,
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
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
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
        let new_ex =
            (ex | WS_EX_LAYERED.0 as i64 | WS_EX_TOOLWINDOW.0 as i64) & !(WS_EX_APPWINDOW.0 as i64);
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

/// Hide a console window. DWMWA_CLOAK is unreliable on
/// ConsoleWindowClass (conhost-drawn, not a normal DWM surface) —
/// SW_HIDE is the reliable console hide. Cloak anyway as a fallback
/// for pseudo-console hosts that do compose.
#[cfg(windows)]
fn hide_console_window(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    set_cloaked(hwnd, true);
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Strip the frame off RA's render window so only the game surface
/// shows even when a core re-centers it on-screen. Removing
/// WS_CAPTION drops the title bar but NOT the HMENU — a menu-less
/// window just redraws the band over the client area, so SetMenu is
/// still required. Idempotent: re-applied by
/// the watchdog whenever the style creeps back or the hwnd is
/// retargeted. Runs only after the capture exists so the style
/// change can't stall boot-time rendering.
#[cfg(windows)]
fn undecorate_retroarch_window(hwnd: HWND) {
    if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as i64;
        let frame = WS_CAPTION.0 as i64
            | WS_THICKFRAME.0 as i64
            | WS_SYSMENU.0 as i64
            | WS_MINIMIZEBOX.0 as i64
            | WS_MAXIMIZEBOX.0 as i64;
        let new = (style & !frame) | WS_POPUP.0 as i64;
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as i64;
        // WS_EX_TOOLWINDOW drops the taskbar/Alt-Tab entry; removing
        // WS_EX_APPWINDOW keeps it out even after a core restyles.
        let new_ex = (ex | WS_EX_TOOLWINDOW.0 as i64) & !(WS_EX_APPWINDOW.0 as i64);
        if new != style || new_ex != ex {
            if new != style {
                SetWindowLongPtrW(hwnd, GWL_STYLE, new as isize);
            }
            if new_ex != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex as isize);
            }
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        // Removing WS_CAPTION doesn't detach the HMENU — Windows
        // redraws the menu band inside the client area and WGC
        // captures it. SetMenu(NULL) is required to actually drop it.
        // Post-attach only, so the send can't stall boot.
        if !GetMenu(hwnd).is_invalid() {
            let _ = SetMenu(hwnd, None);
            let _ = DrawMenuBar(hwnd);
        }
    }
}

/// Win32API.MoveAway — a bare SetWindowPos to the virtual-screen
/// corner and NOTHING else. No toolwindow restyle: SWP_FRAMECHANGED →
/// WM_STYLECHANGED lands in RA's window proc while a core's emu
/// thread is mid-frame. No-op when the window already sits there.
#[cfg(windows)]
fn park_retroarch_window(hwnd: HWND) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let want_x = vx + (vw - 100).max(0);
        let want_y = vy + (vh - 100).max(0);
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        if r.left == want_x && r.top == want_y {
            return;
        }
        let _ = SetWindowPos(
            hwnd,
            None,
            want_x,
            want_y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// RetroArch needs a re-target decision when its render window was
/// recreated — the capture hangs off the destroyed hwnd and gets zero
/// frames otherwise (Lacrima's OnTargetHwndChanged re-attach).
#[cfg(windows)]
fn retroarch_needs_retarget(captured: HWND, resolved: HWND) -> bool {
    !resolved.is_invalid() && resolved != captured
}

/// Lacrima's WindowHandler equivalent for RetroArch, run once per
/// monitor pass:
/// - re-park the capture window — cores re-center/resize it on
///   geometry changes (Panda3DS put it back mid-screen), and a
///   one-shot park visibly pops back on the desktop;
/// - re-attach capture when the render hwnd was destroyed and
///   recreated (fullscreen toggles, driver re-init);
/// - cloak the core's console — Panda3DS AllocConsoles a "Log
///   Console" window inside retroarch.exe. Consoles are conhost-
///   owned, outside the pid sweep; AttachConsole→GetConsoleWindow is
///   the only reliable way to reach one we don't own.
#[cfg(windows)]
fn retroarch_watchdog(
    pid: u32,
    hwnd: &mut HWND,
    capture: &mut wgc::Capture,
    frames: &Arc<Mutex<wgc::FrameSlot>>,
    gpu: &Arc<wgc::GpuState>,
    render: &Arc<wgc::RenderShared>,
    low_latency: bool,
    hwnd_out: &Arc<std::sync::atomic::AtomicUsize>,
) {
    unsafe {
        // No-op when RA has no console, and skipped entirely when
        // this process already owns one (test harnesses).
        if AttachConsole(pid).is_ok() {
            let w = GetConsoleWindow();
            let _ = FreeConsole();
            // FreeConsole closes the console handles Windows just
            // associated with our process — our std handles are left
            // DANGLING, and the next Command::spawn dies in
            // CreateProcess with ERROR_INVALID_HANDLE ("Das Handle
            // ist ungültig", os error 6). A GUI app's std handles
            // should be NULL — restore that so handle-inheritance
            // survives into the next launch.
            for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let _ = SetStdHandle(h, HANDLE::default());
            }
            hide_console_window(w);
        }
        if !IsWindow(Some(*hwnd)).as_bool() {
            let new = resolve_capture_target(pid, Kind::RetroArch);
            if retroarch_needs_retarget(*hwnd, new) {
                // Same order as the initial attach — park before
                // uncloak so a retargeted window can't flash either.
                park_retroarch_window(new);
                set_cloaked(new, false);
                let _ = ShowWindow(new, SW_RESTORE);
                *capture = wgc::Capture::start(
                    new,
                    frames.clone(),
                    gpu.clone(),
                    render.clone(),
                    low_latency,
                );
                // The dead window's capture may have posted an err —
                // clear it so the monitor doesn't kill the fresh
                // attach on the new hwnd.
                frames.lock().unwrap_or_else(|p| p.into_inner()).err = None;
                *hwnd = new;
                hwnd_out.store(new.0 as usize, Ordering::Relaxed);
                undecorate_retroarch_window(new);
            }
            return;
        }
    }
    undecorate_retroarch_window(*hwnd);
    park_retroarch_window(*hwnd);
}

/// Undo prepare+hide — used when capture attach fails so the user can
/// still see/close the emulator window.
#[cfg(windows)]
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
            let _ = SetMenu(
                hwnd,
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                    menu as *mut core::ffi::c_void,
                )),
            );
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
#[cfg(windows)]
fn sweep_hide(
    pid: u32,
    skip: HWND,
    hidden: &mut std::collections::HashMap<usize, SavedWindow>,
    kind: Kind,
) {
    // RetroArchHandler.PrepareProcessForCapture is a NO-OP:
    // "RetroArch uses Vulkan/DX and can fail to render if its window
    // is hidden before capture." That refers to SW_HIDE/ShowWindow
    // hides — DWM cloaking is different: it clears no styles and the
    // window still believes it is visible, so swapchain presents
    // keep flowing. Cloak is therefore safe to apply from spawn —
    // it kills the boot flash entirely; the resolved target is
    // uncloaked right before attach (and only after it is already
    // parked at the corner, so nothing ever flashes on-screen).
    // Cores like Panda3DS AllocConsole a "Log Console" (console
    // windows report the owner's pid on Win11) — those get SW_HIDE,
    // the reliable console hide.
    if kind == Kind::RetroArch {
        for hwnd in process_windows_of(&win_pids(pid, kind), false) {
            if hwnd == skip || hwnd.is_invalid() {
                continue;
            }
            if class_name(hwnd).to_lowercase().contains("console") {
                hide_console_window(hwnd);
            } else {
                set_cloaked(hwnd, true);
            }
        }
        return;
    }
    for hwnd in process_windows_of(&win_pids(pid, kind), false) {
        if hwnd == skip || hwnd.is_invalid() {
            continue;
        }
        // A real HMENU can be attached after the window's first
        // sweep, so re-strip on every pass. (xemu's menubar isn't an
        // HMENU at all — it's ImGui-rendered in-frame, handled by the
        // show_menubar config patch.) SKIPPED for Cemu: SetMenu/
        // DrawMenuBar send messages into wx's UI thread, which is
        // busy through boot/shader compile — the send blocks, the
        // sweep freezes, and nothing ever gets cloaked. CemuHandler's
        // Prepare*ForCapture are no-ops for the same reason.
        if kind != Kind::Cemu {
            strip_menu(hwnd);
        }
        // RPCS3/shadPS4/DuckStation/PCSX2/Dolphin/Cemu/Eden: cloak
        // EVERY window — a DWM cloak hides instantly with no
        // geometry/style change for Qt/SDL to race, so nothing
        // flashes between spawn and capture. (PCSX2's and Dolphin's
        // Lacrima handlers no-op window prep because park/restyle
        // raced Qt and minimizing stalls Vulkan presentation —
        // cloaking does neither, so it still hides safely.) Other
        // kinds park aux windows.
        let cloak = matches!(
            kind,
            Kind::Rpcs3
                | Kind::Shadps4
                | Kind::DuckStation
                | Kind::Pcsx2
                | Kind::Dolphin
                | Kind::Cemu
                | Kind::Eden
        );
        hidden.entry(hwnd.0 as usize).or_insert_with(|| {
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
#[cfg(windows)]
fn send_ctrl_s(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    let key = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
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
#[cfg(windows)]
fn strip_menu(hwnd: HWND) {
    unsafe {
        if !GetMenu(hwnd).is_invalid() {
            let _ = SetMenu(hwnd, None);
            let _ = DrawMenuBar(hwnd);
        }
    }
}

#[cfg(windows)]
fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

#[cfg(windows)]
fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Job guards for emulator processes launched without a Session
/// (launch_setup). Kept for the process lifetime so the kill-on-close
/// job still fires if the app exits or crashes while a setup window
/// is open — the guards themselves only matter at process teardown.
#[cfg(windows)]
static SETUP_JOBS: Mutex<Vec<JobGuard>> = Mutex::new(Vec::new());

/// Lacrima's ResizeWindowToAspectRatioInPlace — resize the captured
/// window's top-level ancestor to `aspect` at ~1920px wide, clamped
/// to the virtual screen (the render child follows its parent's
/// bounds). The window stays parked at its hidden spot — only its
/// size, and so the capture's native aspect, changes.
#[cfg(windows)]
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
/// a dying app can't leave an orphan emulator process. Dolphin gets
/// `-u <exe dir>/User` and PCSX2 `-portable` so the settings UI
/// edits the same managed config the game launches use (Lacrima's
/// BuildSetupStartInfo).
pub fn launch_setup(exe: &str, kind: Option<Kind>) -> Result<(), String> {
    // The gear needs the Qt settings UI — an eden-cli-resolved
    // launcher swaps to its eden.exe sibling (eden-cli has no UI
    // without -g).
    let exe_path = if kind == Some(Kind::Eden) {
        eden_gui_sibling(exe)
    } else {
        PathBuf::from(exe)
    };
    let work_dir = exe_path
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    let mut cmd = Command::new(&exe_path);
    match kind {
        Some(Kind::Dolphin) => {
            let user = ensure_dolphin_setup(&work_dir);
            cmd.arg("-u").arg(&user);
        }
        Some(Kind::Pcsx2) => {
            cmd.arg("-portable");
        }
        // BuildSetupStartInfo: the settings gear opens Cemu in the
        // same portable dir a game launch uses (2.x detects
        // portable/ beside the exe — there is no -c flag).
        Some(Kind::Cemu) => {
            ensure_cemu_setup(&work_dir);
        }
        // The gear opens Eden's Qt settings UI — prime the same
        // qt-config.ini overrides game launches use.
        Some(Kind::Eden) => {
            ensure_eden_setup();
        }
        // Bare retroarch.exe opens the menu — append the same focus/
        // directory overrides a game launch uses so core settings the
        // user changes land consistently.
        Some(Kind::RetroArch) => {
            if let Some(cfg) = retroarch_append_config(Path::new(exe), None) {
                cmd.arg("--appendconfig").arg(cfg);
            }
        }
        _ => {}
    }
    let child = cmd
        .current_dir(work_dir)
        .spawn()
        .map_err(|e| format!("failed to start emulator: {e}"))?;
    #[cfg(windows)]
    if let Some(job) = JobGuard::new(&child) {
        SETUP_JOBS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(job);
    }
    #[cfg(not(windows))]
    drop(child);
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
#[cfg(windows)]
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
        K::Colon | K::Semicolon => 0xBA, // OEM_1 (;:)
        K::Plus | K::Equals => 0xBB,     // OEM_PLUS (=+)
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
#[cfg(windows)]
pub fn key_lparam(vk: u16, pressed: bool, repeat: bool) -> LPARAM {
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
    let ext = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E) as isize;
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
#[cfg(windows)]
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

/// Lacrima's `RetroArchHandler.IsRetroArchErrorLine` — the lines that
/// count as errors when the process dies before creating a window.
/// Called by the windows session-log path + cross-platform tests.
#[allow(dead_code)]
fn retroarch_is_error_line(line: &str) -> bool {
    let l = line.trim().to_lowercase();
    if l.is_empty() {
        return false;
    }
    if l.contains("[error]")
        || l.contains("libretro error")
        || l.contains("required files are missing")
        || l.contains("failed to load")
        || l.contains("cannot initialize")
        || l.contains("missing the bios")
    {
        return true;
    }
    l.contains("not found")
        && (l.contains("_bios") || l.contains("sega_101") || l.contains("mpr-17933"))
}

/// `TryParseRetroArchSystemDirectoryFromLog` — the core's
/// GET_SYSTEM_DIRECTORY env reply names the dir the user has to drop
/// firmware into.
#[allow(dead_code)]
fn retroarch_system_dir_from_log<'a>(lines: &[&'a str]) -> Option<String> {
    const MARKER: &str = "GET_SYSTEM_DIRECTORY:";
    lines.iter().find_map(|line| {
        line.to_lowercase()
            .find(&MARKER.to_lowercase())
            .and_then(|i| {
                let p = line[i + MARKER.len()..].trim().trim_matches('"');
                (!p.is_empty()).then(|| p.to_string())
            })
    })
}

/// `TryExtractRetroArchErrorDetails` + `EnhanceRetroArchErrorDetails` —
/// summary is the last error line, details the whole error block;
/// missing-firmware failures are rewritten into the actionable message
/// (the real Beetle Saturn failure is `Cannot open BIOS file
/// mpr-17933.bin` — the last [ERROR] tail just says content load
/// failed, which is useless as a notice).
#[allow(dead_code)]
fn retroarch_error_notice(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let errors: Vec<String> = lines
        .iter()
        .filter(|l| retroarch_is_error_line(l))
        .map(|l| l.trim().to_string())
        .collect();
    if errors.is_empty() {
        return None;
    }
    let mut details = errors.join("\n");
    let mut summary = errors.last().unwrap().clone();
    let low = details.to_lowercase();
    let saturn = low.contains("saturn_bios")
        || low.contains("sega_101")
        || low.contains("mpr-17933")
        || low.contains("saturn");
    let bios_missing = low.contains("missing the bios")
        || low.contains("cannot open bios file")
        || (low.contains("not found") && saturn);
    if bios_missing {
        summary = if saturn {
            "Sega Saturn BIOS files are missing.".to_string()
        } else {
            "Required BIOS/firmware files are missing.".to_string()
        };
        details.push_str("\n\nCopy legally obtained BIOS dumps into:");
        if let Some(dir) = retroarch_system_dir_from_log(&lines) {
            details.push_str(&format!("\n{dir}"));
        }
        if saturn {
            details.push_str(
                "\nRequired files (at least one region): saturn_bios.bin, sega_101.bin, mpr-17933.bin",
            );
        }
    } else if summary
        .to_lowercase()
        .contains("post-processing shader not found")
        || low.contains("default_pre_post_process.glsl")
    {
        summary = "Dolphin core is missing required shader resources.".to_string();
        details.push_str(
            "\nEnsure RetroArch has the Dolphin system files under \
             system\\dolphin-emu\\Sys\\Shaders\\default_pre_post_process.glsl.",
        );
    }
    Some(format!("{summary}\n{details}"))
}

#[cfg(test)]
mod tests {
    /// Regression: a recreated render window must trigger re-attach —
    /// capturing a destroyed hwnd yields zero frames ("game runs but
    /// never gets captured").
    #[test]
    #[cfg(windows)]
    fn retroarch_retarget_on_recreated_window() {
        use super::HWND;
        let dead = HWND(0x1000usize as _);
        let alive = HWND(0x2000usize as _);
        assert!(super::retroarch_needs_retarget(dead, alive));
        assert!(!super::retroarch_needs_retarget(alive, alive));
        assert!(!super::retroarch_needs_retarget(dead, HWND::default()));
    }

    /// Regression: a Failed capture must end the session — otherwise
    /// the emulator keeps running hidden and uncaptured ("game is
    /// running though"), which is exactly the zombie state a failed
    /// CreateForWindow left RetroArch in.
    #[test]
    fn failed_status_counts_as_finished() {
        assert!(super::status_finished(&super::Status::Exited));
        assert!(super::status_finished(&super::Status::Failed("x".into())));
        assert!(!super::status_finished(&super::Status::Resolving));
        assert!(!super::status_finished(&super::Status::Capturing));
    }

    /// The Beetle Saturn no-BIOS boot ends with the generic
    /// `[ERROR] [Content] Laden des Inhalts fehlgeschlagen` tail — the
    /// actionable line is the `libretro ERROR` BIOS complaint above it.
    /// The notice must surface the missing-BIOS message + the section
    /// system dir, not just the content-load tail.
    #[test]
    fn retroarch_error_notice_surfaces_bios_error() {
        let log = concat!(
            "[INFO] [Environ] GET_SYSTEM_DIRECTORY: \
             \"C:\\\\Emu\\\\SATURN\\\\Libretro\\\\system\".\n",
            "[libretro ERROR] Cannot open BIOS file \
             \"C:\\\\Emu\\\\SATURN\\\\Libretro\\\\system\\\\mpr-17933.bin\".\n",
            "[ERROR] [Content] Laden des Inhalts fehlgeschlagen.\n"
        );
        let note = super::retroarch_error_notice(log).expect("error lines present");
        assert!(note.contains("BIOS"), "{note}");
        assert!(note.contains("mpr-17933"), "{note}");
        assert!(note.contains("Libretro\\\\system"), "{note}");
    }

    /// A generic failure with no enhancement keeps the last [ERROR]
    /// line as the summary and returns all error lines.
    #[test]
    fn retroarch_error_notice_generic_last_error() {
        let log = "[INFO] starting\n[ERROR] [Foo] bar broke\n[INFO] done\n";
        let note = super::retroarch_error_notice(log).expect("error line");
        assert!(note.contains("bar broke"), "{note}");
        assert!(super::retroarch_error_notice("[INFO] all good\n").is_none());
    }

    /// TryGetSaturnBiosValidationError — Beetle Saturn is gated on
    /// saturn_bios.bin/sega_101.bin/mpr-17933.bin in <exe>/system;
    /// non-Saturn-family cores (ymir) skip the check entirely.
    #[test]
    fn saturn_bios_gate() {
        let dir = std::env::temp_dir().join(format!("aes_test_bios_{}", std::process::id()));
        let sys = dir.join("system");
        std::fs::create_dir_all(&sys).unwrap();
        let exe = dir.join("retroarch.exe");
        let beetle = dir.join("mednafen_saturn_libretro.dll");
        let ymir = dir.join("ymir_libretro.dll");
        // Missing → actionable message naming the system dir.
        let e = super::saturn_bios_error(&exe, &beetle).expect("gated");
        assert!(e.contains("BIOS"), "{e}");
        assert!(e.contains("system"), "{e}");
        // Non-saturn-family core → no gate.
        assert!(super::saturn_bios_error(&exe, &ymir).is_none());
        // Any region file releases the gate.
        std::fs::write(sys.join("mpr-17933.bin"), b"x").unwrap();
        assert!(super::saturn_bios_error(&exe, &beetle).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Repro driver for the live RetroArch capture path — run with
    /// `cargo test retroarch_session -- --ignored` and env:
    ///   RA_EXE, RA_CORE, RA_GAME (full paths).
    ///   RA_ROUNDS=N runs N back-to-back sessions (relaunch repro).
    #[test]
    #[ignore]
    fn retroarch_session_repro() {
        let exe = std::env::var("RA_EXE").expect("RA_EXE");
        let core = std::path::PathBuf::from(std::env::var("RA_CORE").expect("RA_CORE"));
        let game = std::path::PathBuf::from(std::env::var("RA_GAME").expect("RA_GAME"));
        let ll = std::env::var("RA_LL").map(|v| v == "1").unwrap_or(true);
        let rounds: u32 = std::env::var("RA_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        for round in 0..rounds {
            let mut s = super::Session::start(
                &exe,
                &game,
                ll,
                None,
                super::Kind::RetroArch,
                Some(core.clone()),
            )
            .expect("session start");
            for i in 0..60 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let st = s.status();
                let st_txt = match &st {
                    super::Status::Resolving => "Resolving".to_string(),
                    super::Status::Capturing => "Capturing".to_string(),
                    super::Status::Exited => "Exited".to_string(),
                    super::Status::Failed(e) => format!("Failed({e})"),
                };
                eprintln!(
                    "round={round} t={}ms status={st_txt} finished={}",
                    i * 250,
                    s.finished()
                );
                if matches!(st, super::Status::Exited | super::Status::Failed(_)) || s.finished() {
                    break;
                }
            }
            s.stop();
            drop(s);
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    }
}
