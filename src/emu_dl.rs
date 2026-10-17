//! Emulator downloads — the Rust port of Lacrima's
//! `XeniaEmulatorUpdateService.DownloadOrUpdateAsync`: fetch the
//! newest Xenia Canary GitHub release, download the windows zip into
//! `<data>/Emulators/<Section>/<Handler>` (`Emulators/X360/Xenia`),
//! extract it, pull the game-patches pack into `patches/`, and mark
//! the installed version — all on a worker thread while the settings
//! card polls `DlProgress`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const XENIA_RELEASES_API: &str =
    "https://api.github.com/repos/xenia-canary/xenia-canary/releases?per_page=20";
const FLYCAST_RELEASES_API: &str =
    "https://api.github.com/repos/flyinghead/flycast/releases?per_page=20";
const XEMU_RELEASES_API: &str =
    "https://api.github.com/repos/xemu-project/xemu/releases?per_page=20";
// RPCS3 doesn't publish on its main repo — the CI ships every build
// to rpcs3-binaries-win instead (Lacrima's BinaryReleasesApiEndpoint).
// Linux builds land in the parallel rpcs3-binaries-linux repo as
// rpcs3-v<ver>-<sha>_linux64.AppImage.
const RPCS3_RELEASES_API: &str =
    "https://api.github.com/repos/RPCS3/rpcs3-binaries-win/releases?per_page=20";
const RPCS3_RELEASES_API_LINUX: &str =
    "https://api.github.com/repos/RPCS3/rpcs3-binaries-linux/releases?per_page=20";
const SHADPS4_RELEASES_API: &str =
    "https://api.github.com/repos/shadps4-emu/shadPS4/releases?per_page=20";
const DUCKSTATION_RELEASES_API: &str =
    "https://api.github.com/repos/stenzek/duckstation/releases?per_page=100";
const PCSX2_RELEASES_API: &str = "https://api.github.com/repos/PCSX2/pcsx2/releases?per_page=100";
// Dolphin publishes no GitHub releases and its own updater endpoints
// are behind a bot challenge — Lacrima's EmuCR fallback is the same
// chain: blog feed → post page → direct dl.dolphin-emu.org asset.
const EMUCR_DOLPHIN_FEED: &str =
    "https://www.emucr.com/feeds/posts/default/-/Dolphin?alt=json&max-results=30";
// CemuEmulatorUpdateService's ReleasesApiEndpoint.
const CEMU_RELEASES_API: &str =
    "https://api.github.com/repos/cemu-project/Cemu/releases?per_page=100";
// EdenEmulatorUpdateService's default repo — Eden publishes on its
// own Forgejo instance, not GitHub; the releases API returns the
// same tag_name/prerelease/assets(browser_download_url) shape the
// generic GitHub parse reads.
const EDEN_RELEASES_API: &str =
    "https://git.eden-emu.dev/api/v1/repos/eden-emu/eden/releases?limit=20";
const GAME_PATCHES_URL: &str =
    "https://github.com/xenia-canary/game-patches/releases/download/latest/game-patches.7z";
const USER_AGENT: &str = "AES_Rustified-XeniaUpdater/1.0";
/// Same agent string, exposed for the RPCS3 patch downloader.
pub(crate) const USER_AGENT_PUBLIC: &str = USER_AGENT;
const XENIA_VERSION_MARKER: &str = "xenia_version.txt";
const FLYCAST_VERSION_MARKER: &str = "flycast_version.txt";
const XEMU_VERSION_MARKER: &str = "xemu_version.txt";
const RPCS3_VERSION_MARKER: &str = "rpcs3_version.txt";
const SHADPS4_VERSION_MARKER: &str = "shadps4_version.txt";
const DUCKSTATION_VERSION_MARKER: &str = "duckstation_version.txt";
const PCSX2_VERSION_MARKER: &str = "pcsx2_version.txt";
const DOLPHIN_VERSION_MARKER: &str = "dolphin_version.txt";
const CEMU_VERSION_MARKER: &str = "cemu_version.txt";
const EDEN_VERSION_MARKER: &str = "eden_version.txt";

/// Shared download state the settings card polls each frame.
#[derive(Default)]
pub struct DlProgress {
    /// Human-readable stage ("Downloading xenia_canary…", …).
    pub stage: String,
    /// 0..=1 within the current download stage.
    pub frac: f32,
    /// Set once: Ok(resolved xenia.exe path) or Err(message).
    pub done: Option<Result<String, String>>,
}

pub type Shared = Arc<Mutex<DlProgress>>;

/// `<data>/Emulators/X360/Xenia` — Lacrima's
/// `ApplicationPaths.EmulatorsDirectory/<SectionKey>/Xenia`.
pub fn xenia_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("X360").join("Xenia"))
}

/// `<data>/Emulators/DC/Flycast` — same layout for the Dreamcast
/// handler.
pub fn flycast_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("DC").join("Flycast"))
}

/// `<data>/Emulators/XBOX/Xemu` — Lacrima's
/// `EmulatorsDirectory/<SectionKey>/Xemu`.
pub fn xemu_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("XBOX").join("Xemu"))
}

/// `<data>/Emulators/PS3/RPCS3` — Lacrima's
/// `EmulatorsDirectory/PS3/RPCS3`.
pub fn rpcs3_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("PS3").join("RPCS3"))
}

/// `<data>/Emulators/PS4/shadPS4` — same layout for the PS4 handler.
pub fn shadps4_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("PS4").join("shadPS4"))
}

/// `<data>/Emulators/PSX/DuckStation` — Lacrima's
/// `EmulatorsDirectory/PSX/DuckStation`.
pub fn duckstation_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("PSX").join("DuckStation"))
}

/// `<data>/Emulators/PS2/PCSX2`.
pub fn pcsx2_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("PS2").join("PCSX2"))
}

/// `<data>/Emulators/GCN/Dolphin` — one managed install serves both
/// the GameCube and Wii sections (DolphinHandler covers both).
pub fn dolphin_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("GCN").join("Dolphin"))
}

/// Find the installed launcher (`ResolveLauncherPath`): the Windows
/// exes first, then the shared resolver covers the Linux AppImage
/// (`xenia_canary_linux.AppImage` matches `exe_contains: "xenia"`).
pub fn xenia_exe() -> Option<PathBuf> {
    resolve_exe(&xenia_dir()?, &XENIA_SPEC).ok()
}

/// Find the installed `flycast.exe` under the managed dir.
pub fn flycast_exe() -> Option<PathBuf> {
    resolve_exe(&flycast_dir()?, &FLYCAST_SPEC).ok()
}

/// Find the installed `xemu.exe` under the managed dir.
pub fn xemu_exe() -> Option<PathBuf> {
    resolve_exe(&xemu_dir()?, &XEMU_SPEC).ok()
}

/// Find the installed `rpcs3.exe` under the managed dir.
pub fn rpcs3_exe() -> Option<PathBuf> {
    resolve_exe(&rpcs3_dir()?, &RPCS3_SPEC).ok()
}

/// Find the installed `shadPS4.exe` under the managed dir.
pub fn shadps4_exe() -> Option<PathBuf> {
    resolve_exe(&shadps4_dir()?, &SHADPS4_SPEC).ok()
}

/// Find the installed DuckStation exe under the managed dir.
pub fn duckstation_exe() -> Option<PathBuf> {
    resolve_exe(&duckstation_dir()?, &DUCKSTATION_SPEC).ok()
}

/// Find the installed PCSX2 exe under the managed dir.
pub fn pcsx2_exe() -> Option<PathBuf> {
    resolve_exe(&pcsx2_dir()?, &PCSX2_SPEC).ok()
}

/// Find the installed `Dolphin.exe` under the managed dir.
pub fn dolphin_exe() -> Option<PathBuf> {
    resolve_exe(&dolphin_dir()?, &DOLPHIN_SPEC).ok()
}

/// `<data>/Emulators/WIIU/Cemu` — CemuEmulatorUpdateService's
/// EnsureDirectories.
pub fn cemu_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("WIIU").join("Cemu"))
}

/// `<data>/Emulators/SWITCH/Eden` — EdenEmulatorUpdateService's
/// EnsureDirectories.
pub fn eden_dir() -> Option<PathBuf> {
    crate::settings::data_root().map(|d| d.join("Emulators").join("SWITCH").join("Eden"))
}

/// `<data>/Emulators/RetroArch` — the shared RetroArch frontend
/// install (RetroArchEmulatorUpdateService installs to
/// `Emulators/Retroarch` — a common dir, not a console section).
/// Every RetroArch-backed section resolves this same frontend; only
/// the Libretro core sets are section-specific —
/// `Emulators/<SECTION>/Libretro`. The frontend used to live under
/// `Emulators/3DS/` — existing installs are migrated in place.
pub fn retroarch_dir() -> Option<PathBuf> {
    let dir = crate::settings::data_root()?.join("Emulators").join("RetroArch");
    if !dir.exists() {
        let legacy = dir.parent()?.join("3DS").join("RetroArch");
        if legacy.is_dir() {
            let _ = std::fs::rename(&legacy, &dir);
        }
    }
    Some(dir)
}

/// Case-insensitive `strip_prefix` — Windows paths compare
/// case-insensitively on disk, so a configured path that differs in
/// casing from `data_root()` still counts as inside the legacy tree.
fn strip_prefix_ci(p: &Path, base: &Path) -> Option<PathBuf> {
    let mut pc = p.components();
    for bc in base.components() {
        let ok = pc
            .next()
            .is_some_and(|c| {
                c.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&bc.as_os_str().to_string_lossy())
            });
        if !ok {
            return None;
        }
    }
    Some(pc.as_path().to_path_buf())
}

/// Configured-path half of the 3DS→common move: a saved `retroarch`
/// emulator path under the legacy `Emulators/3DS/RetroArch` tree is
/// repointed at the moved install. Without this the settings card
/// keeps showing (and resolution keeps trying first) a path into a
/// directory the folder migration emptied.
/// Returns the remapped path, or None when `configured` doesn't live
/// under the legacy dir.
pub fn retroarch_migrate_configured_path(configured: &str) -> Option<String> {
    let root = crate::settings::data_root()?;
    let legacy = root.join("Emulators").join("3DS").join("RetroArch");
    let rest = strip_prefix_ci(Path::new(configured.trim()), &legacy)?;
    Some(
        root.join("Emulators")
            .join("RetroArch")
            .join(rest)
            .to_string_lossy()
            .into_owned(),
    )
}

/// Find `retroarch.exe` / `retroarch` under the managed install.
/// Lacrima's ResolveLauncherPath walks the emulator dir for the
/// launcher binary; macOS app bundles resolve their inner binary.
/// The top level is checked first — the full walk runs ~10k files
/// on a RetroArch install and callers poll it every frame.
pub fn retroarch_exe() -> Option<PathBuf> {
    let dir = retroarch_dir()?;
    for want in ["retroarch.exe", "retroarch"] {
        let p = dir.join(want);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut all = Vec::new();
    walk(&dir, &mut all);
    for want in ["retroarch.exe", "RetroArch.exe", "retroarch", "RetroArch"] {
        if let Some(p) = all.iter().find(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.eq_ignore_ascii_case(want))
        }) {
            return Some(p.clone());
        }
    }
    // Loose match like resolve_exe's fallback — exe-shaped names
    // only (retroarch_version.txt would otherwise win and spawn fails
    // with ENOEXEC), newest first.
    let exe = all
        .into_iter()
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|f| f.to_str()).is_some_and(|f| {
                    let low = f.to_lowercase();
                    low.contains("retroarch")
                        && (low.ends_with(".exe")
                            || (cfg!(unix)
                                && (low.ends_with(".appimage") || !low.contains('.'))))
                        && arch_ok(&low)
                })
        })
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })?;
    // A user-placed binary may have lost its exec bit (downloads and
    // zip extraction don't preserve it) — resolution must yield a
    // runnable file.
    mark_exec(&exe);
    Some(exe)
}

/// Find the installed `Cemu.exe` under the managed dir.
pub fn cemu_exe() -> Option<PathBuf> {
    resolve_exe(&cemu_dir()?, &CEMU_SPEC).ok()
}

/// Launch resolution — EdenEmulatorUpdateService's
/// ResolveLauncherPath prefers `eden-cli.exe` (its stdout emits the
/// "RenderReady" token Lacrima's capture gate waits on, and it boots
/// straight into the game without the Qt shell), then `eden.exe`.
pub fn eden_exe() -> Option<PathBuf> {
    resolve_exe(&eden_dir()?, &EDEN_SPEC).ok()
}

/// The settings gear wants the Qt GUI, not the game-boot cli exe —
/// `eden-cli.exe` with no `-g` exits printing usage. (The gear
/// resolves via `eden_exe` then swaps the filename in
/// `xenia::launch_setup`; kept for a direct gui-path lookup.)
#[allow(dead_code)]
pub fn eden_gui_exe() -> Option<PathBuf> {
    let dir = eden_dir()?;
    let mut all = Vec::new();
    walk(&dir, &mut all);
    all.into_iter()
        .find(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.eq_ignore_ascii_case("eden.exe"))
        })
        .or_else(eden_exe)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Depth-first search for a directory named `name`.
fn find_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return None;
    };
    let mut nested = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.file_name().and_then(|f| f.to_str()) == Some(name) {
            return Some(p);
        }
        nested.push(p);
    }
    nested.iter().find_map(|d| find_dir(d, name))
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .build(),
    )
}

fn set(dl: &Shared, stage: &str, frac: f32) {
    let mut p = dl.lock().unwrap_or_else(|e| e.into_inner());
    p.stage = stage.to_string();
    p.frac = frac;
}

/// Kick off the download/install — returns immediately; poll the
/// shared progress until `done` is set.
/// Standalone "Download patches" for the patches overlay —
/// game-patches.zip → `<root>/patches` (same archive the emulator
/// Download path installs via `install_patches`).
pub fn download_patches(root: PathBuf) -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: "Downloading game patches…".into(),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        let res = (|| -> Result<String, String> {
            let update_dir = root.join("Emu_Update");
            std::fs::create_dir_all(&update_dir).map_err(|e| format!("Dir not writable: {e}"))?;
            install_patches(&shared, &root, &update_dir)?;
            let _ = std::fs::remove_dir_all(&update_dir);
            Ok("Patches installed.".to_string())
        })();
        shared.lock().unwrap_or_else(|e| e.into_inner()).done = Some(res);
    });
    dl
}

/// Handler-agnostic download spec — which release feed, which asset
/// marks the platform build, which exe names the install is complete.
/// False when an asset name carries a CPU-arch marker the host can't
/// run — release listings mix arm64/aarch64/armhf/riscv builds into the
/// same list as x86_64 ones and a bare `contains("linux")` picks
/// whichever sorts first (the armhf DuckStation AppImage got selected
/// on x86_64 exactly this way). Marker-less names pass — several
/// emulators ship a single unmarked Linux build.
fn arch_ok(n: &str) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        ![
            "aarch64", "arm64", "armhf", "armv7", "armv6", "armv5", "arm32", "-arm.",
            "riscv", "ppc64", "mips", "loongarch",
        ]
        .iter()
        .any(|m| n.contains(m))
    }
    #[cfg(target_arch = "aarch64")]
    {
        !["x86_64", "amd64", "x64", "win64", "i386", "i686"]
            .iter()
            .any(|m| n.contains(m))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = n;
        true
    }
}

struct EmuSpec {
    name: &'static str,
    releases_api: &'static str,
    version_marker: &'static str,
    dir: fn() -> Option<PathBuf>,
    /// True when the release asset is this platform's build
    /// (branches on `cfg!` internally — most emulators ship a
    /// Linux AppImage or tarball alongside the windows zip).
    asset_ok: fn(&str) -> bool,
    /// Preferred exe filenames, in order, then any exe containing
    /// `exe_contains`. The list holds both platforms' names —
    /// entries are matched exactly, so foreign-platform names
    /// simply never hit.
    exe_names: &'static [&'static str],
    exe_contains: &'static str,
    /// Pull the game-patches pack too (Xenia only).
    patches: bool,
    /// Accept `prerelease` releases — PCSX2 marks every rolling
    /// build prerelease, so a strict filter finds nothing.
    allow_prerelease: bool,
}

const XENIA_SPEC: EmuSpec = EmuSpec {
    name: "Xenia",
    releases_api: XENIA_RELEASES_API,
    version_marker: XENIA_VERSION_MARKER,
    dir: xenia_dir,
    asset_ok: |n| {
        let n = n.to_lowercase();
        // Canary ships `xenia_canary_windows.7z` and an experimental
        // `xenia_canary_linux.AppImage`.
        if cfg!(windows) {
            n.contains("windows") && n.contains("canary") && arch_ok(&n)
        } else {
            n.contains("canary") && n.ends_with(".appimage") && arch_ok(&n)
        }
    },
    exe_names: &["xenia_canary.exe", "xenia.exe", "xenia_canary", "xenia"],
    exe_contains: "xenia",
    patches: true,
    allow_prerelease: false,
};

const FLYCAST_SPEC: EmuSpec = EmuSpec {
    name: "Flycast",
    releases_api: FLYCAST_RELEASES_API,
    version_marker: FLYCAST_VERSION_MARKER,
    dir: flycast_dir,
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            (n.contains("win64") || n.contains("windows")) && arch_ok(&n)
        } else {
            (n.contains("linux") || n.contains("appimage")) && arch_ok(&n)
        }
    },
    exe_names: &["flycast.exe", "flycast"],
    exe_contains: "flycast",
    patches: false,
    allow_prerelease: false,
};

const XEMU_SPEC: EmuSpec = EmuSpec {
    name: "xemu",
    releases_api: XEMU_RELEASES_API,
    version_marker: XEMU_VERSION_MARKER,
    dir: xemu_dir,
    // Lacrima's SelectAssetForPlatform: xemu-win-x86_64-release.zip
    // first, then any win+x86_64 zip that isn't debug symbols.
    // On Linux current releases ship xemu-<ver>-<arch>.AppImage (no
    // "linux"/"ubuntu" marker); older releases had ubuntu-* tarballs.
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            (n == "xemu-win-x86_64-release.zip" || (n.contains("win") && n.contains("x86_64")))
                && !n.contains("dbg")
                && !n.contains("pdb")
        } else {
            (n.ends_with(".appimage") || n.contains("linux") || n.contains("ubuntu"))
                && !n.contains("dbg")
                && !n.contains("source")
                && arch_ok(&n)
        }
    },
    exe_names: &["xemu.exe", "xemu"],
    exe_contains: "xemu",
    patches: false,
    allow_prerelease: false,
};

pub fn download_xenia() -> Shared {
    download_emu(&XENIA_SPEC)
}

pub fn download_flycast() -> Shared {
    download_emu(&FLYCAST_SPEC)
}

const RPCS3_SPEC: EmuSpec = EmuSpec {
    name: "RPCS3",
    releases_api: if cfg!(windows) {
        RPCS3_RELEASES_API
    } else {
        RPCS3_RELEASES_API_LINUX
    },
    version_marker: RPCS3_VERSION_MARKER,
    dir: rpcs3_dir,
    // Lacrima's SelectAssetForPlatform: a windows archive minus the
    // symbols payload (rpcs3-v<ver>-<sha>_win64.7z). The Linux build
    // is an AppImage (rpcs3-v<ver>-<sha>_linux.AppImage).
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            n.contains("win") && !n.contains("symbols") && arch_ok(&n)
        } else {
            n.contains("linux") && n.ends_with(".appimage") && arch_ok(&n)
        }
    },
    exe_names: &["rpcs3.exe", "rpcs3"],
    exe_contains: "rpcs3",
    patches: false,
    allow_prerelease: false,
};

pub fn download_xemu() -> Shared {
    download_emu(&XEMU_SPEC)
}

pub fn download_rpcs3() -> Shared {
    download_emu(&RPCS3_SPEC)
}

const SHADPS4_SPEC: EmuSpec = EmuSpec {
    name: "shadPS4",
    releases_api: SHADPS4_RELEASES_API,
    version_marker: SHADPS4_VERSION_MARKER,
    dir: shadps4_dir,
    // Lacrima's SelectAssetForPlatform: the windows Qt zip first
    // (older releases), the win64-sdl zip on current ones. Linux
    // builds are zips/appimages marked linux.
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            n.contains("win") && n.ends_with(".zip") && arch_ok(&n)
        } else {
            n.contains("linux") && !n.contains("source") && arch_ok(&n)
        }
    },
    exe_names: &[
        "shadPS4.exe",
        "shadps4.exe",
        "shadPS4QtLauncher.exe",
        "shadps4",
        "ShadPs4QtLauncher",
    ],
    exe_contains: "shad",
    patches: false,
    allow_prerelease: false,
};

pub fn download_shadps4() -> Shared {
    download_emu(&SHADPS4_SPEC)
}

const DUCKSTATION_SPEC: EmuSpec = EmuSpec {
    name: "DuckStation",
    releases_api: DUCKSTATION_RELEASES_API,
    version_marker: DUCKSTATION_VERSION_MARKER,
    dir: duckstation_dir,
    // Lacrima's asset pick: duckstation-windows-x64-release.zip —
    // the symbols 7z and the sse2/x64 variants match a naive
    // windows+x64 filter and sort BEFORE the real zip in the
    // release's asset list, so exclude them explicitly.
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            n.contains("duckstation-windows")
                && n.contains("x64")
                && !n.contains("symbols")
                && !n.contains("sse2")
                && !n.contains("arm64")
        } else {
            // DuckStation-x64.AppImage / linux release archives —
            // arch_ok rejects the armhf/arm64 AppImages, `sse2` the
            // legacy-instruction-set x64 build.
            (n.contains("appimage") || n.contains("linux"))
                && !n.contains("source")
                && !n.contains("sse2")
                && arch_ok(&n)
        }
    },
    // ResolveLauncherPath — the LTCG build first, then the plain Qt
    // exe and renames.
    exe_names: &[
        "duckstation-qt-x64-ReleaseLTCG.exe",
        "duckstation-qt-x64-Release.exe",
        "duckstation-qt.exe",
        "duckstation.exe",
        "duckstation-qt",
        "duckstation",
    ],
    exe_contains: "duckstation",
    patches: false,
    allow_prerelease: false,
};

pub fn download_duckstation() -> Shared {
    download_emu(&DUCKSTATION_SPEC)
}

const PCSX2_SPEC: EmuSpec = EmuSpec {
    name: "PCSX2",
    releases_api: PCSX2_RELEASES_API,
    version_marker: PCSX2_VERSION_MARKER,
    dir: pcsx2_dir,
    // SelectAssetForPlatform: the windows Qt 7z minus symbols
    // (pcsx2-v<ver>-windows-x64-Qt.7z). Linux ships an AppImage
    // (pcsx2-v<ver>-linux-appimage-x64-Qt.AppImage).
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            n.contains("windows") && n.contains("qt") && !n.contains("symbols") && n.ends_with(".7z")
        } else {
            n.contains("linux") && n.contains("x64") && !n.contains("symbols") && arch_ok(&n)
        }
    },
    exe_names: &[
        "pcsx2-qt.exe",
        "pcsx2-qtx64-avx2.exe",
        "pcsx2-qtx64.exe",
        "pcsx2.exe",
        "pcsx2-qt",
        "pcsx2",
    ],
    exe_contains: "pcsx2",
    patches: false,
    allow_prerelease: true,
};

pub fn download_pcsx2() -> Shared {
    download_emu(&PCSX2_SPEC)
}

const DOLPHIN_SPEC: EmuSpec = EmuSpec {
    name: "Dolphin",
    // No GitHub feed — work_dolphin scrapes EmuCR posts instead.
    releases_api: EMUCR_DOLPHIN_FEED,
    version_marker: DOLPHIN_VERSION_MARKER,
    dir: dolphin_dir,
    // dl.dolphin-emu.org asset names: dolphin-<ver>-x64.7z.
    // Dolphin ships no official Linux binaries — nothing matches
    // off Windows and the user gets the browse-instead path.
    asset_ok: |n| {
        let n = n.to_lowercase();
        cfg!(windows)
            && n.contains("dolphin")
            && n.contains("x64")
            && (n.ends_with(".7z") || n.ends_with(".zip"))
    },
    // DolphinHandler's DolphinExecutableNames.
    exe_names: &[
        "Dolphin.exe",
        "dolphin.exe",
        "DolphinQt2.exe",
        "dolphinqt2.exe",
        "DolphinWx.exe",
        "dolphinwx.exe",
        "Dolphin-emu.exe",
        "dolphin-emu.exe",
        "dolphin-emu",
        "dolphin",
    ],
    exe_contains: "dolphin",
    patches: false,
    allow_prerelease: false,
};

pub fn download_dolphin() -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: "Starting Dolphin download…".into(),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        #[cfg(windows)]
        let res = work_dolphin(&shared, &DOLPHIN_SPEC);
        #[cfg(not(windows))]
        let res = work_dolphin_linux(&shared, &DOLPHIN_SPEC);
        let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
        p.done = Some(res);
    });
    dl
}

/// Linux: Dolphin ships no official Linux binaries — the supported
/// distribution is the Flathub flatpak. Install it (user scope, no
/// sudo) when missing, then drop a `dolphin` wrapper script into the
/// managed dir — `resolve_exe` treats it like any other emu exe and
/// the launch args pass straight through `flatpak run`.
#[cfg(not(windows))]
fn work_dolphin_linux(dl: &Shared, spec: &EmuSpec) -> Result<String, String> {
    use std::process::Command;

    const APP: &str = "org.DolphinEmu.dolphin-emu";

    let Some(emu_dir) = (spec.dir)() else {
        return Err("Could not resolve the emulators directory".into());
    };
    std::fs::create_dir_all(&emu_dir).map_err(|e| format!("Emu dir not writable: {e}"))?;

    let flatpak_ok = Command::new("flatpak")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !flatpak_ok {
        return Err(
            "Dolphin ships no Linux binaries — the official package is the Flathub \
             flatpak (org.DolphinEmu.dolphin-emu) and flatpak isn't installed here. \
             Install flatpak, or browse to an existing Dolphin binary."
                .into(),
        );
    }

    let info = Command::new("flatpak")
        .args(["info", APP])
        .output()
        .map_err(|e| format!("flatpak info failed: {e}"))?;
    if !info.status.success() {
        set(dl, "Installing Dolphin (Flathub)…", 0.0);
        let out = Command::new("flatpak")
            .args(["install", "-y", "--user", "flathub", APP])
            .output()
            .map_err(|e| format!("flatpak install failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "flatpak install failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    // Marker: the Version line from `flatpak info`, else "flatpak".
    let stdout = String::from_utf8_lossy(&info.stdout);
    let tag = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("Version:").map(str::trim))
        .filter(|s| !s.is_empty())
        .unwrap_or("flatpak")
        .to_string();

    // --filesystem=host: the flatpak's default permissions only cover
    // a few xdg-run paths — the game ROM and our portable `-u User`
    // dir would be invisible inside the sandbox without it.
    let wrapper = emu_dir.join("dolphin");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec flatpak run --filesystem=host {APP} \"$@\"\n"),
    )
    .map_err(|e| format!("Wrapper write failed: {e}"))?;
    mark_exec(&wrapper);
    let _ = std::fs::write(emu_dir.join(spec.version_marker), &tag);

    set(dl, "Done", 1.0);
    let exe = resolve_exe(&emu_dir, spec)?;
    mark_exec(&exe);
    Ok(exe.to_string_lossy().into_owned())
}

const CEMU_SPEC: EmuSpec = EmuSpec {
    name: "Cemu",
    releases_api: CEMU_RELEASES_API,
    version_marker: CEMU_VERSION_MARKER,
    dir: cemu_dir,
    // SelectAssetForPlatform: cemu-<ver>-windows-x64.zip — the
    // windows archive. Linux builds are
    // Cemu-<ver>-linux-x86_64.AppImage.
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            n.contains("windows") && n.ends_with(".zip")
        } else {
            n.contains("linux") && n.contains("x86_64") && arch_ok(&n)
        }
    },
    // CemuHandler's CemuExecutableNames.
    exe_names: &["Cemu.exe", "cemu.exe", "Cemu", "cemu"],
    exe_contains: "cemu",
    patches: false,
    allow_prerelease: false,
};

pub fn download_cemu() -> Shared {
    download_emu(&CEMU_SPEC)
}

const EDEN_SPEC: EmuSpec = EmuSpec {
    name: "Eden",
    releases_api: EDEN_RELEASES_API,
    version_marker: EDEN_VERSION_MARKER,
    dir: eden_dir,
    // SelectAssetForPlatform: Lacrima's windows+x64 filter actually
    // misses Eden's real names — the zips are
    // `Eden-Windows-v<ver>-amd64-<toolchain>.zip` — so it lands on
    // the first `win`+.zip by list order (amd64-clang-pgo). Match the
    // desktop amd64 builds explicitly; arm64/rog-ally/zsync/torrent
    // fail the amd64+zip pair on their own.
    asset_ok: |n| {
        let n = n.to_lowercase();
        if cfg!(windows) {
            n.contains("windows") && (n.contains("amd64") || n.contains("x64")) && n.ends_with(".zip")
        } else {
            // Eden-Linux-v<ver>-amd64-<toolchain>.AppImage.
            n.contains("linux") && n.contains("amd64") && !n.contains("zsync") && arch_ok(&n)
        }
    },
    // ResolveLauncherPath — eden-cli.exe first, then the Qt gui.
    exe_names: &["eden-cli.exe", "eden.exe", "eden-cli", "eden"],
    exe_contains: "eden",
    patches: false,
    allow_prerelease: false,
};

pub fn download_eden() -> Shared {
    download_emu(&EDEN_SPEC)
}

/// Kick off the managed RetroArch install — the buildbot **stable**
/// `RetroArch.7z` per platform. The nightly MinGW build is not used:
/// its frontend segfaults under the citra-family cores on Windows.
/// (macOS only ships a .dmg — no managed download there.)
pub fn download_retroarch() -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: "Starting RetroArch download…".into(),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        let res = work_retroarch(&shared);
        let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
        p.done = Some(res);
    });
    dl
}

const RETROARCH_STABLE_FALLBACK: &str = "1.22.2";

/// Newest `X.Y.Z` dir in the buildbot stable listing — parse the
/// h5ai index like Lacrima's nightly scraper, pick the max version
/// tuple. Returns the pinned fallback when the listing is
/// unreachable or holds no version dirs.
fn retroarch_stable_version() -> String {
    let Ok(html) = get_text("https://buildbot.libretro.com/stable/") else {
        return RETROARCH_STABLE_FALLBACK.to_string();
    };
    best_stable_version(&html).unwrap_or_else(|| RETROARCH_STABLE_FALLBACK.to_string())
}

/// Max `X.Y.Z` dir among `href="/stable/<ver>/"` entries — a plain
/// lexicographic max would rank 1.9.9 above 1.22.2, so compare
/// version tuples.
fn best_stable_version(html: &str) -> Option<String> {
    let mut best = (0u64, 0u64, 0u64);
    let mut rest = html;
    while let Some(at) = rest.find("/stable/") {
        rest = &rest[at + 8..];
        let end = rest.find('/').unwrap_or(0);
        let ver = &rest[..end];
        let mut parts = ver.split('.');
        if let (Some(a), Some(b), Some(c)) = (
            parts.next().and_then(|p| p.parse().ok()),
            parts.next().and_then(|p| p.parse().ok()),
            parts.next().and_then(|p| p.parse().ok()),
        ) {
            best = best.max((a, b, c));
        }
        rest = &rest[end..];
    }
    (best != (0, 0, 0)).then(|| format!("{}.{}.{}", best.0, best.1, best.2))
}

/// The platform's RetroArch stable main-emulator archive for
/// `version`. macOS only ships a .dmg, which our extractor can't
/// open — the user installs it and browses instead.
fn retroarch_stable_url(version: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        return Some(format!(
            "https://buildbot.libretro.com/stable/{version}/windows/x86_64/RetroArch.7z"
        ));
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some(format!(
            "https://buildbot.libretro.com/stable/{version}/linux/x86_64/RetroArch.7z"
        ));
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Some(format!(
            "https://buildbot.libretro.com/stable/{version}/linux/aarch64/RetroArch.7z"
        ));
    }
    #[allow(unreachable_code)]
    let _ = version;
    None
}

fn work_retroarch(dl: &Shared) -> Result<String, String> {
    let Some(emu_dir) = retroarch_dir() else {
        return Err("Could not resolve the emulators directory".into());
    };
    set(dl, "Checking RetroArch releases…", 0.0);
    let version = retroarch_stable_version();
    let url = retroarch_stable_url(&version).ok_or_else(|| {
        "No managed RetroArch build for this platform — install RetroArch and Browse to it"
            .to_string()
    })?;
    let update_dir = emu_dir.join("Emu_Update");
    std::fs::create_dir_all(&update_dir).map_err(|e| format!("Emu dir not writable: {e}"))?;

    let asset_name = url.rsplit('/').next().unwrap_or("RetroArch.7z");
    let zip_path = update_dir.join(asset_name);
    download(
        dl,
        &format!("Downloading RetroArch {version}…"),
        &url,
        &zip_path,
    )?;

    set(dl, "Extracting RetroArch…", 0.0);
    let extracted = update_dir.join("extracted");
    let _ = std::fs::remove_dir_all(&extracted);
    std::fs::create_dir_all(&extracted).map_err(|e| format!("Extract failed: {e}"))?;
    extract_archive(&zip_path, &extracted)?;
    let root = normalize_root(&extracted);
    copy_dir_contents(&root, &emu_dir, "Emu_Update")?;

    let _ = std::fs::write(emu_dir.join("retroarch_version.txt"), &version);
    let _ = std::fs::remove_dir_all(&update_dir);
    let _ = std::fs::create_dir_all(&update_dir);

    set(dl, "Done", 1.0);
    let exe = retroarch_exe()
        .ok_or_else(|| "retroarch executable not found after install".to_string())?;
    // Extracted binaries lose the exec bit (Lacrima's
    // TryMarkRetroArchLinuxBinaryExecutable).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
    }
    Ok(exe.to_string_lossy().into_owned())
}

/// Lacrima's DolphinEmulatorUpdateService EmuCR fallback:
/// feed entries → post HTML → the dl.dolphin-emu.org windows asset.
/// "Dolphin XXXX[a]" posts are beta-track releases, "Dolphin Git
/// XXXX-N" are dev builds — try the release posts first.
#[cfg(windows)]
fn work_dolphin(dl: &Shared, spec: &EmuSpec) -> Result<String, String> {
    let Some(emu_dir) = (spec.dir)() else {
        return Err("Could not resolve the emulators directory".into());
    };
    let update_dir = emu_dir.join("Emu_Update");
    std::fs::create_dir_all(&update_dir).map_err(|e| format!("Emu dir not writable: {e}"))?;

    set(dl, "Fetching Dolphin builds…", 0.0);
    let feed = get_text(spec.releases_api)?;
    let json: serde_json::Value =
        serde_json::from_str(&feed).map_err(|e| format!("Bad feed json: {e}"))?;
    let mut posts: Vec<(String, String)> = json
        .get("feed")
        .and_then(|f| f.get("entry"))
        .and_then(|e| e.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let title = e.get("title")?.get("$t")?.as_str()?.to_string();
                    let url = e
                        .get("link")?
                        .as_array()?
                        .iter()
                        .find(|l| l.get("rel").and_then(|r| r.as_str()) == Some("alternate"))?
                        .get("href")?
                        .as_str()?
                        .to_string();
                    Some((title, url))
                })
                .collect()
        })
        .unwrap_or_default();
    if posts.is_empty() {
        return Err("No Dolphin builds found on the feed".into());
    }
    // Release posts ("Dolphin 2609a") before dev posts ("Dolphin Git
    // 2609-81") — same beta-vs-dev split as the updater's tracks.
    posts.sort_by_key(|(t, _)| t.to_lowercase().contains("git"));

    // Each post links the dl.dolphin-emu.org archive directly; scan a
    // few until one yields a windows x64 asset.
    let mut asset: Option<(String, String)> = None;
    for (title, url) in posts.iter().take(8) {
        set(dl, &format!("Scanning {title}…"), 0.0);
        let Ok(html) = get_text(url) else {
            continue;
        };
        if let Some(a) = emucr_dolphin_asset(&html, spec) {
            asset = Some(a);
            break;
        }
    }
    let (asset_name, asset_url) =
        asset.ok_or_else(|| "No compatible Dolphin asset found".to_string())?;
    // Marker tag: the /releases/<ver>/ or /builds/<maj>/<min>/ path
    // component, else the asset stem.
    let tag = asset_url
        .split('/')
        .nth_back(1)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| asset_name.trim_end_matches(".7z").to_string());

    let stage = format!("Downloading {asset_name}…");
    let zip_path = update_dir.join(&asset_name);
    download(dl, &stage, &asset_url, &zip_path)?;

    set(dl, "Extracting Dolphin…", 0.0);
    let extracted = update_dir.join("extracted");
    let _ = std::fs::remove_dir_all(&extracted);
    std::fs::create_dir_all(&extracted).map_err(|e| format!("Extract failed: {e}"))?;
    extract_archive(&zip_path, &extracted)?;
    let root = normalize_root(&extracted);
    copy_dir_contents(&root, &emu_dir, "Emu_Update")?;

    let _ = std::fs::write(emu_dir.join(spec.version_marker), tag.trim());
    let _ = std::fs::remove_dir_all(&update_dir);
    let _ = std::fs::create_dir_all(&update_dir);

    set(dl, "Done", 1.0);
    let exe = resolve_exe(&emu_dir, spec)?;
    mark_exec(&exe);
    Ok(exe.to_string_lossy().into_owned())
}

fn get_text(url: &str) -> Result<String, String> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("Fetch failed: {e}"))?;
    resp.body_mut()
        .read_to_string()
        .map_err(|e| format!("Fetch failed: {e}"))
}

/// First `dl.dolphin-emu.org` href in an EmuCR post that matches the
/// spec's windows-asset predicate (dolphin-<ver>-x64.7z — the .apk and
/// arm/macos links fail the predicate).
#[cfg(any(windows, test))]
fn emucr_dolphin_asset(html: &str, spec: &EmuSpec) -> Option<(String, String)> {
    let mut rest = html;
    while let Some(at) = rest.find("href=\"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let url = &rest[..end];
        let low = url.to_lowercase();
        if !low.contains("dl.dolphin-emu.org") {
            continue;
        }
        let name = url.rsplit('/').next().unwrap_or(url);
        if (spec.asset_ok)(name) {
            return Some((name.to_string(), url.to_string()));
        }
    }
    None
}

/// The newest non-prerelease release object for a spec (same
/// fetch + sort the install path uses). The update-check worker and
/// `work` share this so "latest" always means the same thing.
fn fetch_latest_release(spec: &EmuSpec) -> Result<serde_json::Value, String> {
    let mut resp = agent()
        .get(spec.releases_api)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Release lookup failed: {e}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Release lookup failed: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Bad release json: {e}"))?;
    let releases = json.as_array().cloned().unwrap_or_default();
    let mut releases: Vec<serde_json::Value> = releases
        .into_iter()
        .filter(|r| {
            spec.allow_prerelease || r.get("prerelease").and_then(|p| p.as_bool()) != Some(true)
        })
        .collect();
    releases.sort_by(|a, b| {
        let pa = a.get("published_at").and_then(|p| p.as_str()).unwrap_or("");
        let pb = b.get("published_at").and_then(|p| p.as_str()).unwrap_or("");
        pb.cmp(pa)
    });
    releases
        .into_iter()
        .next()
        .ok_or_else(|| format!("No {} releases found", spec.name))
}

// ======================================================================
// Update check — Emulation view's section-entry prompt
// ======================================================================

/// Result of a background update check — the section prompt polls
/// `done` each frame. `installed` is the managed-install marker,
/// `latest` the newest upstream tag; `None` where a side couldn't
/// be determined (no marker / lookup failed / no checkable source).
#[derive(Default)]
pub struct VerCheck {
    pub done: Option<(Option<String>, Option<String>)>,
}

pub type VerShared = Arc<Mutex<VerCheck>>;

/// The managed install's version marker contents, if present.
fn marker_version(dir: Option<PathBuf>, marker: &str) -> Option<String> {
    let v = std::fs::read_to_string(dir?.join(marker)).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Handler id → its spec (update checks key off the same ids the
/// settings cards do). Dolphin has no managed release feed and
/// RetroArch checks a buildbot listing instead of a releases API.
fn spec_for_handler(handler: &str) -> Option<&'static EmuSpec> {
    Some(match handler {
        "xenia" => &XENIA_SPEC,
        "flycast" => &FLYCAST_SPEC,
        "xemu" => &XEMU_SPEC,
        "rpcs3" => &RPCS3_SPEC,
        "shadps4" => &SHADPS4_SPEC,
        "duckstation" => &DUCKSTATION_SPEC,
        "pcsx2" => &PCSX2_SPEC,
        "cemu" => &CEMU_SPEC,
        "eden" => &EDEN_SPEC,
        _ => return None,
    })
}

/// The installed version string for a handler — the marker file the
/// download worker writes into the managed emu dir.
pub fn installed_version(handler: &str) -> Option<String> {
    if handler == "retroarch" {
        return marker_version(retroarch_dir(), "retroarch_version.txt");
    }
    let spec = spec_for_handler(handler)?;
    marker_version((spec.dir)(), spec.version_marker)
}

/// Spawn a worker that resolves (installed, latest) for a handler —
/// cheap metadata only, no download. Handlers without a checkable
/// source (Dolphin's EmuCR chain, flatpak) report `latest: None`.
pub fn update_check(handler: &str) -> VerShared {
    let shared: VerShared = Arc::new(Mutex::new(VerCheck::default()));
    let worker = shared.clone();
    let handler = handler.to_string();
    std::thread::spawn(move || {
        let installed = installed_version(&handler);
        let latest = if handler == "retroarch" {
            Some(retroarch_stable_version())
        } else {
            spec_for_handler(&handler)
                .and_then(|spec| {
                    fetch_latest_release(spec).ok().and_then(|r| {
                        r.get("tag_name").and_then(|t| t.as_str()).map(str::to_string)
                    })
                })
        };
        worker.lock().unwrap_or_else(|e| e.into_inner()).done = Some((installed, latest));
    });
    shared
}

fn download_emu(spec: &'static EmuSpec) -> Shared {
    let dl: Shared = Arc::new(Mutex::new(DlProgress {
        stage: format!("Starting {} download…", spec.name),
        ..Default::default()
    }));
    let shared = dl.clone();
    std::thread::spawn(move || {
        let res = work(&shared, spec);
        let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
        p.done = Some(res);
    });
    dl
}

fn work(dl: &Shared, spec: &EmuSpec) -> Result<String, String> {
    let Some(emu_dir) = (spec.dir)() else {
        return Err("Could not resolve the emulators directory".into());
    };
    let update_dir = emu_dir.join("Emu_Update");
    std::fs::create_dir_all(&update_dir).map_err(|e| format!("Emu dir not writable: {e}"))?;

    // Fetch releases — newest non-prerelease first.
    set(dl, &format!("Fetching {} releases…", spec.name), 0.0);
    let release = fetch_latest_release(spec)?;
    let tag = release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Pick the windows archive (Lacrima's SelectAssetForPlatform —
    // Xenia ships .7z, Flycast a win64 .zip).
    let assets: Vec<(String, String)> = release
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|n| {
                    Some((
                        n.get("name")?.as_str()?.to_string(),
                        n.get("browser_download_url")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let is_archive = |n: &str| {
        let n = n.to_lowercase();
        n.ends_with(".zip")
            || n.ends_with(".7z")
            || (cfg!(unix)
                && (n.ends_with(".tar.gz")
                    || n.ends_with(".tgz")
                    || n.ends_with(".tar.xz")
                    || n.ends_with(".tar.zst")
                    || n.ends_with(".tar.bz2")))
    };
    let pick = |pred: &dyn Fn(&str) -> bool| assets.iter().find(|(n, _)| pred(n)).cloned();
    // Windows picks must land on an archive (the extractor only
    // handles zip/7z there); Linux picks may be a bare AppImage or a
    // tar.* the system tar unpacks.
    #[cfg(windows)]
    let picked = pick(&|n: &str| (spec.asset_ok)(n) && is_archive(n))
        .or_else(|| pick(&|n: &str| n.to_lowercase().contains("win") && is_archive(n)))
        .or_else(|| pick(&|n: &str| is_archive(n)));
    #[cfg(not(windows))]
    let picked = pick(&|n: &str| (spec.asset_ok)(n)).or_else(|| {
        pick(&|n: &str| {
            let n = n.to_lowercase();
            is_archive(&n) && (n.contains("linux") || n.contains("ubuntu"))
        })
    });
    let (asset_name, asset_url) =
        picked.ok_or_else(|| format!("No compatible {} asset found", spec.name))?;

    // Download the asset.
    let stage = format!("Downloading {asset_name} ({tag})…");
    let zip_path = update_dir.join(&asset_name);
    download(dl, &stage, &asset_url, &zip_path)?;

    // Archives extract to a staging dir, normalize a single root
    // folder, then copy into the emulator dir (skipping Emu_Update
    // payloads). Bare binaries/AppImages drop straight into the
    // emulator dir and get the exec bit.
    set(dl, &format!("Extracting {}…", spec.name), 0.0);
    if is_archive(&asset_name) {
        let extracted = update_dir.join("extracted");
        let _ = std::fs::remove_dir_all(&extracted);
        std::fs::create_dir_all(&extracted).map_err(|e| format!("Extract failed: {e}"))?;
        extract_archive(&zip_path, &extracted)?;
        let root = normalize_root(&extracted);
        copy_dir_contents(&root, &emu_dir, "Emu_Update")?;
    } else {
        let dest = emu_dir.join(&asset_name);
        std::fs::copy(&zip_path, &dest).map_err(|e| format!("Install failed: {e}"))?;
        mark_exec(&dest);
    }

    // Game patches — best effort (Lacrima tolerates failure here too).
    if spec.patches {
        set(dl, "Downloading game patches…", 0.0);
        let _ = install_patches(dl, &emu_dir, &update_dir);
    }

    // Version marker + cleanup.
    let _ = std::fs::write(emu_dir.join(spec.version_marker), tag.trim());
    let _ = std::fs::remove_dir_all(&update_dir);
    let _ = std::fs::create_dir_all(&update_dir);

    set(dl, "Done", 1.0);
    let exe = resolve_exe(&emu_dir, spec)?;
    mark_exec(&exe);
    Ok(exe.to_string_lossy().into_owned())
}

/// Preferred exe names in order, then any exe containing
/// `spec.exe_contains` under the dir.
fn resolve_exe(dir: &Path, spec: &EmuSpec) -> Result<PathBuf, String> {
    let mut all = Vec::new();
    walk(dir, &mut all);
    for want in spec.exe_names {
        if let Some(p) = all.iter().find(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.eq_ignore_ascii_case(want))
        }) {
            return Ok(p.clone());
        }
    }
    // Prefer the newest match — updated installs leave versioned
    // binaries (rpcs3-vX.AppImage, DuckStation-x64-SSE2.AppImage)
    // alongside the fresh download, and an arbitrary pick can land
    // on the stale one.
    all.into_iter()
        .filter(|p| {
            p.file_name().and_then(|f| f.to_str()).is_some_and(|f| {
                let low = f.to_lowercase();
                low.contains(spec.exe_contains)
                    && (low.ends_with(".exe")
                        || (cfg!(unix) && (low.ends_with(".appimage") || !low.contains('.'))))
                    && arch_ok(&low)
            })
        })
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .ok_or_else(|| format!("{} exe not found after install", spec.name))
}

/// Set the exec bit on a unix binary — downloads and zip/7z
/// extraction don't preserve it (Lacrima's
/// TryMarkRetroArchLinuxBinaryExecutable).
#[cfg(unix)]
fn mark_exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(mut perm) = p.metadata().map(|m| m.permissions()) {
        if perm.mode() & 0o111 != 0o111 {
            perm.set_mode(perm.mode() | 0o111);
            let _ = std::fs::set_permissions(p, perm);
        }
    }
}

#[cfg(not(unix))]
fn mark_exec(_p: &Path) {}

fn download(dl: &Shared, stage: &str, url: &str, dest: &Path) -> Result<(), String> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("{stage} failed: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut f = std::fs::File::create(dest).map_err(|e| format!("{stage} failed: {e}"))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 16384];
    let mut got = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("{stage} failed: {e}"))?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])
            .map_err(|e| format!("{stage} failed: {e}"))?;
        got += n as u64;
        set(
            dl,
            stage,
            total
                .map(|t| got as f32 / t as f32)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0),
        );
    }
    Ok(())
}

/// Zip via `zip`, 7z via `sevenz_rust2` — falling back to the
/// system `tar.exe` (Windows' bsdtar reads 7z) like Lacrima's
/// ArchiveExtractionHelper does when managed extraction fails.
/// Unix tarballs go through the system `tar` (GNU tar auto-detects
/// gz/xz/zst/bz2 by content).
fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let name = archive
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or_default()
        .to_lowercase();
    #[cfg(unix)]
    if name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
        || name.ends_with(".tar.xz")
        || name.ends_with(".tar.zst")
        || name.ends_with(".tar.bz2")
    {
        let ok = std::process::Command::new("tar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        return ok.then_some(()).ok_or_else(|| "tar extract failed".to_string());
    }
    let is_7z = archive
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("7z"));
    if is_7z {
        if let Err(e) = sevenz_rust2::decompress_file(archive, dest) {
            // System-tool fallback (Lacrima's TryExtract7zWithSystemTool).
            // Call Windows' bsdtar by full path — a bare "tar" can
            // resolve to GNU tar (no 7z support) on dev machines.
            #[cfg(windows)]
            let ok = std::process::Command::new(r"C:\Windows\System32\tar.exe")
                .arg("-xf")
                .arg(archive)
                .arg("-C")
                .arg(dest)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            // On unix, bsdtar reads 7z too; GNU tar doesn't, but
            // sevenz_rust2 already handled the archive when present.
            #[cfg(unix)]
            let ok = ["7z", "7za", "tar"].iter().any(|tool| {
                let mut cmd = std::process::Command::new(tool);
                if *tool == "tar" {
                    cmd.arg("-xf").arg(archive).arg("-C").arg(dest);
                } else {
                    cmd.arg("x").arg("-y").arg(format!("-o{}", dest.display())).arg(archive);
                }
                cmd.output().map(|o| o.status.success()).unwrap_or(false)
            });
            if !ok {
                return Err(format!("Extract failed: {e}"));
            }
        }
        return Ok(());
    }
    extract_zip(archive, dest)
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path).map_err(|e| format!("Invalid zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(f).map_err(|e| format!("Invalid zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Extract failed: {e}"))?;
        let Some(name) = entry.enclosed_name().map(|n| n.to_path_buf()) else {
            continue;
        };
        let out = dest.join(name);
        if entry.is_dir() {
            let _ = std::fs::create_dir_all(&out);
            continue;
        }
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut w = std::fs::File::create(&out).map_err(|e| format!("Extract failed: {e}"))?;
        std::io::copy(&mut entry, &mut w).map_err(|e| format!("Extract failed: {e}"))?;
    }
    Ok(())
}

/// If the archive ships a single root folder, descend into it.
fn normalize_root(extracted: &Path) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(extracted)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    let has_files = std::fs::read_dir(extracted)
        .map(|rd| rd.flatten().any(|e| e.path().is_file()))
        .unwrap_or(false);
    if dirs.len() == 1 && !has_files {
        dirs[0].clone()
    } else {
        extracted.to_path_buf()
    }
}

fn copy_dir_contents(src: &Path, dst: &Path, skip_prefix: &str) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("Copy failed: {e}"))?;
    let mut files = Vec::new();
    walk(src, &mut files);
    for f in files {
        let rel = f.strip_prefix(src).unwrap_or(&f);
        if rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .is_some_and(|c| c.eq_ignore_ascii_case(skip_prefix))
        {
            continue;
        }
        let out = dst.join(rel);
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(&f, &out).map_err(|e| format!("Copy failed: {e}"))?;
    }
    Ok(())
}

/// `EnsureGamePatchesAsync`: fetch game-patches.zip, find the
/// `patches` dir inside, and replace `<emu_dir>/patches` with it.
fn install_patches(dl: &Shared, emu_dir: &Path, update_dir: &Path) -> Result<(), String> {
    // The game-patches repo ships a .7z on the "latest" tag.
    let arc_path = update_dir.join("game-patches.7z");
    download(dl, "Downloading game patches…", GAME_PATCHES_URL, &arc_path)?;
    let extracted = update_dir.join("patches_extracted");
    let _ = std::fs::remove_dir_all(&extracted);
    std::fs::create_dir_all(&extracted).map_err(|e| format!("Patch extract failed: {e}"))?;
    extract_archive(&arc_path, &extracted)?;

    // The archive may root at `patches/` or nest it one level down.
    let src = if extracted.join("patches").is_dir() {
        extracted.join("patches")
    } else {
        find_dir(&extracted, "patches").ok_or_else(|| "No patches folder in archive".to_string())?
    };
    let dst = emu_dir.join("patches");
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir_contents(&src, &dst, "")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EmuCR post pages mix the windows archive with apk/arm links —
    /// the picker must land on dolphin-<ver>-x64.7z specifically.
    #[test]
    fn emucr_post_links_pick_windows_x64() {
        let html = r#"
            <a href="https://dl.dolphin-emu.org/releases/2609a/dolphin-2609a.apk">android</a>
            <a href="https://dl.dolphin-emu.org/releases/2609a/dolphin-2609a-x64.7z">windows</a>
            <a href="https://dl.dolphin-emu.org/releases/2609a/dolphin-2609a-arm64.7z">arm</a>
            <a href="https://dl.dolphin-emu.org/releases/2609a/dolphin-2609a-x64.dmg">mac</a>
        "#;
        if cfg!(windows) {
            let (name, url) = emucr_dolphin_asset(html, &DOLPHIN_SPEC).unwrap();
            assert_eq!(name, "dolphin-2609a-x64.7z");
            assert_eq!(
                url,
                "https://dl.dolphin-emu.org/releases/2609a/dolphin-2609a-x64.7z"
            );
        } else {
            // Dolphin ships no official Linux build — nothing matches.
            assert!(emucr_dolphin_asset(html, &DOLPHIN_SPEC).is_none());
        }
        assert!(emucr_dolphin_asset("<a href=\"https://example.com/x\">", &DOLPHIN_SPEC).is_none());
    }

    /// RetroArch stable channel — the newest `X.Y.Z` dir wins, with
    /// tuple (not lexicographic) ordering and the fallback on junk.
    #[test]
    fn retroarch_stable_version_prefers_newest_tuple() {
        let html = r#"
            <a href="/stable/1.9.9/">1.9.9</a>
            <a href="/stable/1.22.2/">1.22.2</a>
            <a href="/stable/1.22.1/">1.22.1</a>
            <a href="/stable/altstore.json">altstore.json</a>
            <a href="/nightly/">nightly</a>
        "#;
        assert_eq!(best_stable_version(html).as_deref(), Some("1.22.2"));
        assert_eq!(best_stable_version("<a href=\"/stable/x\">"), None);
    }

    /// Legacy→common frontend migration: a configured path under
    /// `Emulators/3DS/RetroArch` remaps to `Emulators/RetroArch`
    /// (case-insensitive prefix, Windows semantics); anything outside
    /// the legacy tree is left alone.
    #[test]
    fn retroarch_configured_path_migration() {
        let root = crate::settings::data_root().expect("data root");
        let legacy = root.join("Emulators").join("3DS").join("RetroArch");
        let old = legacy.join("retroarch.exe").to_string_lossy().into_owned();
        let moved = retroarch_migrate_configured_path(&old).expect("remaps");
        let expect = root
            .join("Emulators")
            .join("RetroArch")
            .join("retroarch.exe")
            .to_string_lossy()
            .into_owned();
        assert_eq!(moved, expect);
        // Casing differences in the saved path still migrate.
        let odd = old.replace("3DS", "3ds");
        assert_eq!(retroarch_migrate_configured_path(&odd).as_deref(), Some(expect.as_str()));
        // Unrelated configured paths are untouched.
        assert!(retroarch_migrate_configured_path("D:\\Tools\\retroarch.exe").is_none());
    }

    /// SelectAssetForPlatform — Cemu ships windows/macos/linux per
    /// release; only this platform's asset must pass the filter.
    #[test]
    fn cemu_asset_prefers_windows_zip() {
        let ok = |n: &str| (CEMU_SPEC.asset_ok)(n);
        if cfg!(windows) {
            assert!(ok("cemu-2.6-windows-x64.zip"));
            assert!(!ok("cemu-2.6-macos-15-arm64.dmg"));
            assert!(!ok("cemu-2.6-linux-x86_64.zip"));
            assert!(!ok("cemu-2.6-windows-x64.zip.sig"));
        } else {
            assert!(ok("Cemu-2.6-linux-x86_64.AppImage"));
            assert!(!ok("cemu-2.6-windows-x64.zip"));
            assert!(!ok("cemu-2.6-macos-15-arm64.dmg"));
        }
    }

    /// arch_ok is the load-bearing guard — release listings mix
    /// foreign-arch builds into the same list and the picker used to
    /// grab the armhf DuckStation AppImage on x86_64. Every spec's
    /// predicate routes through it, so a marker that slips past one
    /// platform check still dies here.
    #[test]
    fn arch_ok_rejects_foreign_arches() {
        #[cfg(target_arch = "x86_64")]
        {
            for bad in [
                "DuckStation-armhf.AppImage",
                "DuckStation-aarch64.AppImage",
                "duckstation-linux-arm64.zip",
                "flycast-arm64.AppImage",
                "rpcs3-v1.0-linux_aarch64.AppImage",
                "xemu-ubuntu-armv7.tar.gz",
                "thing-riscv64.AppImage",
            ] {
                assert!(!arch_ok(bad), "{bad} should be rejected");
            }
            for good in [
                "DuckStation-x64.AppImage",
                "Cemu-2.6-linux-x86_64.AppImage",
                "rpcs3-v1.0-linux.AppImage",
                "Eden-Linux-v0.1-amd64-gcc.AppImage",
                "xemu-0.8.66-ubuntu-24.04.tar.gz",
            ] {
                assert!(arch_ok(good), "{good} should pass");
            }
            // The DuckStation/xemu predicates specifically — these
            // exercise the LINUX asset_ok branches, so they only hold
            // off-Windows (the Windows branch rightly rejects
            // AppImages). The DuckStation armhf rejection is the
            // exact bug the user hit.
            #[cfg(unix)]
            {
                assert!(!(DUCKSTATION_SPEC.asset_ok)("DuckStation-armhf.AppImage"));
                assert!(!(DUCKSTATION_SPEC.asset_ok)("DuckStation-arm64.AppImage"));
                assert!(!(DUCKSTATION_SPEC.asset_ok)("DuckStation-x64-SSE2.AppImage"));
                assert!((DUCKSTATION_SPEC.asset_ok)("DuckStation-x64.AppImage"));
                // xemu dropped the "ubuntu"/"linux" marker from asset
                // names — the Linux build is now xemu-<ver>-x86_64.AppImage.
                assert!((XEMU_SPEC.asset_ok)("xemu-0.8.137-x86_64.AppImage"));
                assert!(!(XEMU_SPEC.asset_ok)("xemu-0.8.137-aarch64.AppImage"));
                assert!(!(XEMU_SPEC.asset_ok)("xemu-0.8.137-dbg-x86_64.AppImage"));
                assert!(!(XEMU_SPEC.asset_ok)("xemu-0.8.137-windows-x86_64.zip"));
            }
        }
    }
}
