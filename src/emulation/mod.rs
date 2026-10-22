//! Emulator subsystem: the Emulation/Decomp views plus the per-emulator
//! handlers, config/profile/patch helpers, overlays, and the capture
//! plumbing (WGC on Windows, the nested Wayland compositor elsewhere).

pub mod cap_shader;
pub mod cemu_gpack;
pub mod cemu_overlay;
pub mod decomp;
pub mod emu_dl;
pub mod emu_meta;
pub mod libretro;
pub mod pillarbox;
pub mod rom_editor;
pub mod rpcs3_overlay;
pub mod rpcs3_patch;
pub mod shadps4;
pub mod shadps4_overlay;
pub mod wgc;
#[cfg(not(windows))]
pub mod wlcomp;
pub mod xenia;
pub mod xenia_cfg;
pub mod xenia_overlay;
pub mod xenia_profile;

// The Emulation view prototype — Lacrima's EmulationView chrome:
// `Grid RowDefinitions="*,Auto,Auto"` = the item view on top, the
// 140px control band (section-list toggle left, cover-layout toggle
// right), and the collapsible console-section strip at the bottom.
// The item area is a placeholder until ROM scanning lands; the
// settings tab renders the Emulation sections with the console cards
// expandable but no handler controls wired yet.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Pos2, Rect, RichText,
    Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};

use crate::{album_row, carousel, library, paint, search_pill, settings, transport};

// The input tunnel only exists on Windows (PostMessage into the
// hidden emulator window) — off-Windows the emulator runs in its own
// window and handles input natively.
#[cfg(windows)]
use windows::Win32::Foundation::{LPARAM, WPARAM};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL,
};

/// Section keys that have a playable handler — Xenia/X360,
/// Flycast/DC and xemu/XBOX are wired; the other cards stay
/// browse/collect placeholders.
const XENIA_SECTION: &str = "X360";
const XENIA_HANDLER: &str = "xenia";
const DC_SECTION: &str = "DC";
const FLYCAST_HANDLER: &str = "flycast";
const XBOX_SECTION: &str = "XBOX";
const XEMU_HANDLER: &str = "xemu";
const PS3_SECTION: &str = "PS3";
const RPCS3_HANDLER: &str = "rpcs3";
const PS4_SECTION: &str = "PS4";
const SHADPS4_HANDLER: &str = "shadps4";
const PSX_SECTION: &str = "PSX";
const DUCKSTATION_HANDLER: &str = "duckstation";
const PS2_SECTION: &str = "PS2";
const PCSX2_HANDLER: &str = "pcsx2";
const GCN_SECTION: &str = "GCN";
const WII_SECTION: &str = "WII";
const DOLPHIN_HANDLER: &str = "dolphin";
const WIIU_SECTION: &str = "WIIU";
const CEMU_HANDLER: &str = "cemu";
const SWITCH_SECTION: &str = "SWITCH";
const EDEN_HANDLER: &str = "eden";
const THREE_DS_SECTION: &str = "3DS";
const N64_SECTION: &str = "N64";
const NES_SECTION: &str = "NES";
const GENESIS_SECTION: &str = "GENESIS";
const SNES_SECTION: &str = "SNES";
const GBA_SECTION: &str = "GBA";
const PSP_SECTION: &str = "PSP";
const SATURN_SECTION: &str = "SATURN";
const FBN_SECTION: &str = "FBN";
const ARCADE_SECTION: &str = "ARCADE";
const RETROARCH_HANDLER: &str = "retroarch";

/// The persisted selected-core field for a RetroArch-backed section —
/// each section keeps its own choice; empty means the first
/// installed core by display name.
fn selected_core<'a>(
    cfg: &'a mut settings::Emulation,
    section: libretro::Section,
) -> &'a mut String {
    match section {
        libretro::Section::ThreeDs => &mut cfg.selected_3ds_core,
        libretro::Section::N64 => &mut cfg.selected_n64_core,
        libretro::Section::Nes => &mut cfg.selected_nes_core,
        libretro::Section::Genesis => &mut cfg.selected_genesis_core,
        libretro::Section::Snes => &mut cfg.selected_snes_core,
        libretro::Section::Gba => &mut cfg.selected_gba_core,
        libretro::Section::Psp => &mut cfg.selected_psp_core,
        libretro::Section::Saturn => &mut cfg.selected_saturn_core,
        libretro::Section::FbNeo => &mut cfg.selected_fbn_core,
        libretro::Section::Arcade => &mut cfg.selected_arcade_core,
    }
}

/// Read-only form for launch — `launch()` only borrows `cfg`.
fn selected_core_id<'a>(cfg: &'a settings::Emulation, section: libretro::Section) -> &'a str {
    match section {
        libretro::Section::ThreeDs => cfg.selected_3ds_core.as_str(),
        libretro::Section::N64 => cfg.selected_n64_core.as_str(),
        libretro::Section::Nes => cfg.selected_nes_core.as_str(),
        libretro::Section::Genesis => cfg.selected_genesis_core.as_str(),
        libretro::Section::Snes => cfg.selected_snes_core.as_str(),
        libretro::Section::Gba => cfg.selected_gba_core.as_str(),
        libretro::Section::Psp => cfg.selected_psp_core.as_str(),
        libretro::Section::Saturn => cfg.selected_saturn_core.as_str(),
        libretro::Section::FbNeo => cfg.selected_fbn_core.as_str(),
        libretro::Section::Arcade => cfg.selected_arcade_core.as_str(),
    }
}

/// The settings-card handler row for a section — the emulator's
/// display name, managed-install lookup, and download starter
/// (Lacrima's per-section `EmulationHandlerAppItem`).
struct HandlerSpec {
    /// Section key the row defaults to ("X360", "DC").
    section: &'static str,
    /// `cfg.emulators` / `default_handlers` id ("xenia", "flycast").
    handler: &'static str,
    /// Display name ("Xenia", "Flycast").
    name: &'static str,
    /// Textbox hint for the launcher path.
    exe_hint: &'static str,
    /// File-dialog title for Browse.
    dialog_title: &'static str,
    /// Managed-install exe lookup (`emu_dl::*_exe`).
    installed_exe: fn() -> Option<PathBuf>,
    /// Kick the GitHub-release install (`emu_dl::download_*`).
    download: fn() -> emu_dl::Shared,
}

fn handler_spec(section_key: &str) -> Option<HandlerSpec> {
    Some(match section_key {
        XENIA_SECTION => HandlerSpec {
            section: XENIA_SECTION,
            handler: XENIA_HANDLER,
            name: "Xenia",
            exe_hint: "Path to xenia.exe",
            dialog_title: "Locate xenia.exe",
            installed_exe: emu_dl::xenia_exe,
            download: emu_dl::download_xenia,
        },
        DC_SECTION => HandlerSpec {
            section: DC_SECTION,
            handler: FLYCAST_HANDLER,
            name: "Flycast",
            exe_hint: "Path to flycast.exe",
            dialog_title: "Locate flycast.exe",
            installed_exe: emu_dl::flycast_exe,
            download: emu_dl::download_flycast,
        },
        XBOX_SECTION => HandlerSpec {
            section: XBOX_SECTION,
            handler: XEMU_HANDLER,
            name: "xemu",
            exe_hint: "Path to xemu.exe",
            dialog_title: "Locate xemu.exe",
            installed_exe: emu_dl::xemu_exe,
            download: emu_dl::download_xemu,
        },
        PS3_SECTION => HandlerSpec {
            section: PS3_SECTION,
            handler: RPCS3_HANDLER,
            name: "RPCS3",
            exe_hint: "Path to rpcs3.exe",
            dialog_title: "Locate rpcs3.exe",
            installed_exe: emu_dl::rpcs3_exe,
            download: emu_dl::download_rpcs3,
        },
        PS4_SECTION => HandlerSpec {
            section: PS4_SECTION,
            handler: SHADPS4_HANDLER,
            name: "shadPS4",
            exe_hint: "Path to shadPS4.exe",
            dialog_title: "Locate shadPS4.exe",
            installed_exe: emu_dl::shadps4_exe,
            download: emu_dl::download_shadps4,
        },
        PSX_SECTION => HandlerSpec {
            section: PSX_SECTION,
            handler: DUCKSTATION_HANDLER,
            name: "DuckStation",
            exe_hint: "Path to duckstation-qt-x64-ReleaseLTCG.exe",
            dialog_title: "Locate DuckStation",
            installed_exe: emu_dl::duckstation_exe,
            download: emu_dl::download_duckstation,
        },
        PS2_SECTION => HandlerSpec {
            section: PS2_SECTION,
            handler: PCSX2_HANDLER,
            name: "PCSX2",
            exe_hint: "Path to pcsx2-qt.exe",
            dialog_title: "Locate pcsx2-qt.exe",
            installed_exe: emu_dl::pcsx2_exe,
            download: emu_dl::download_pcsx2,
        },
        // GameCube + Wii share one Dolphin install — Lacrima's
        // DolphinHandler covers both album titles.
        GCN_SECTION | WII_SECTION => HandlerSpec {
            section: GCN_SECTION,
            handler: DOLPHIN_HANDLER,
            name: "Dolphin",
            exe_hint: "Path to Dolphin.exe",
            dialog_title: "Locate Dolphin.exe",
            installed_exe: emu_dl::dolphin_exe,
            download: emu_dl::download_dolphin,
        },
        WIIU_SECTION => HandlerSpec {
            section: WIIU_SECTION,
            handler: CEMU_HANDLER,
            name: "Cemu",
            exe_hint: "Path to Cemu.exe",
            dialog_title: "Locate Cemu.exe",
            installed_exe: emu_dl::cemu_exe,
            download: emu_dl::download_cemu,
        },
        SWITCH_SECTION => HandlerSpec {
            section: SWITCH_SECTION,
            handler: EDEN_HANDLER,
            name: "Eden",
            exe_hint: "Path to eden.exe",
            dialog_title: "Locate eden.exe",
            installed_exe: emu_dl::eden_exe,
            download: emu_dl::download_eden,
        },
        THREE_DS_SECTION => retroarch_spec(THREE_DS_SECTION),
        // N64, NES, Genesis, SNES + GBA share the RetroArch handler +
        // frontend install — only the Libretro core set is
        // section-specific.
        N64_SECTION => retroarch_spec(N64_SECTION),
        NES_SECTION => retroarch_spec(NES_SECTION),
        GENESIS_SECTION => retroarch_spec(GENESIS_SECTION),
        SNES_SECTION => retroarch_spec(SNES_SECTION),
        GBA_SECTION => retroarch_spec(GBA_SECTION),
        PSP_SECTION => retroarch_spec(PSP_SECTION),
        SATURN_SECTION => retroarch_spec(SATURN_SECTION),
        FBN_SECTION => retroarch_spec(FBN_SECTION),
        ARCADE_SECTION => retroarch_spec(ARCADE_SECTION),
        _ => return None,
    })
}

/// Every RetroArch-backed section shows the same handler row — one
/// shared frontend install; only the Libretro core set differs.
fn retroarch_spec(section: &'static str) -> HandlerSpec {
    HandlerSpec {
        section,
        handler: RETROARCH_HANDLER,
        name: "RetroArch",
        exe_hint: "Path to retroarch.exe",
        dialog_title: "Locate retroarch.exe",
        installed_exe: emu_dl::retroarch_exe,
        download: emu_dl::download_retroarch,
    }
}

/// Handler id → launch kind. Shared by `launch` and the setup-gear
/// so both resolve the same executable flavor.
fn handler_kind(handler: Option<&str>) -> Option<xenia::Kind> {
    Some(match handler {
        Some(XENIA_HANDLER) => xenia::Kind::Xenia,
        Some(FLYCAST_HANDLER) => xenia::Kind::Flycast,
        Some(XEMU_HANDLER) => xenia::Kind::Xemu,
        Some(RPCS3_HANDLER) => xenia::Kind::Rpcs3,
        Some(SHADPS4_HANDLER) => xenia::Kind::Shadps4,
        Some(DUCKSTATION_HANDLER) => xenia::Kind::DuckStation,
        Some(PCSX2_HANDLER) => xenia::Kind::Pcsx2,
        Some(DOLPHIN_HANDLER) => xenia::Kind::Dolphin,
        Some(CEMU_HANDLER) => xenia::Kind::Cemu,
        Some(EDEN_HANDLER) => xenia::Kind::Eden,
        Some(RETROARCH_HANDLER) => xenia::Kind::RetroArch,
        _ => return None,
    })
}

/// Lacrima's EmulationConsoleCatalog FilePatterns — what a section's
/// handler accepts (the handler *is* the file-type filter). Archives
/// are appended to every section like `GetScanPatterns` does.
fn section_patterns(key: &str) -> &'static [&'static str] {
    match key {
        "SNES" => &["sfc", "smc", "fig", "swc"],
        "NES" => &["nes", "fds", "unf", "unif"],
        "N64" => &["n64", "z64", "v64"],
        "DC" => &["cdi", "gdi", "chd", "cue", "iso"],
        "GCN" => &[
            "iso", "gcm", "ciso", "gcz", "rvz", "wia", "dol", "elf", "tgc",
        ],
        "GBA" => &["gba"],
        "GENESIS" => &["gen", "md", "smd", "32x", "sg", "bin", "zip", "7z"],
        "3DS" => &["3ds", "3dsx", "cci", "cxi", "cia"],
        "WII" => &["iso", "wbfs", "wad"],
        "WIIU" => &["wud", "wux", "wua", "rpx"],
        "SWITCH" => &["xci", "nsp", "nsz", "nca"],
        "PSX" => &["cue", "bin", "img", "iso", "chd", "pbp", "m3u"],
        "PS2" => &["iso", "bin", "img", "mdf", "nrg", "chd"],
        "PS3" => &["iso", "pkg", "ps3"],
        "PS4" => &["pkg"],
        "ARCADE" => &["chd", "zip", "7z"],
        "XBOX" => &["iso", "xbe", "xiso"],
        "X360" => &["iso", "xex"],
        "FBN" => &["zip", "7z", "dat"],
        "SATURN" => &["cue", "chd", "iso", "bin", "img", "zip", "7z"],
        "PSP" => &["iso", "cso", "pbp", "chd"],
        _ => &[],
    }
}

const ARCHIVE_EXTS: &[&str] = &["zip", "7z", "rar"];

/// A ROM's cover: the fetched box-art sidecar when one exists —
/// otherwise `None` so the card falls back to the console art.
fn rom_cover(p: &Path) -> Option<library::CoverSrc> {
    let c = emu_meta::cover_path(p);
    c.exists().then_some(library::CoverSrc::File(c))
}

/// The console art a section tile falls back to.
fn console_art(art_dir: Option<&Path>, key: &str) -> Vec<library::CoverSrc> {
    let Some(stem) = CONSOLES.iter().find(|(k, _, _)| *k == key).map(|c| c.2) else {
        return Vec::new();
    };
    if stem.is_empty() {
        return Vec::new();
    }
    art_dir
        .map(|d| vec![library::CoverSrc::File(d.join(format!("{stem}.png")))])
        .unwrap_or_default()
}

/// The section tile's catalog key — stored in the synthetic
/// `emu:<key>` dir so strip reordering can't break key↔tile mapping.
fn section_key(sec: &library::Album) -> String {
    sec.dir
        .to_str()
        .and_then(|d| d.strip_prefix("emu:"))
        .unwrap_or_default()
        .to_string()
}

/// Console sections — (key, display title, art asset stem). Lacrima
/// *discovers* sections by enumerating Assets/Consoles (a console with
/// no art file gets no card — Nintendo DS has none), orders them by
/// filename (that's why GameCube's `GC.png` precedes `Genesis.png`),
/// and derives titles via the catalog's display-name lookup.
const CONSOLES: &[(&str, &str, &str)] = &[
    ("3DS", "Nintendo 3DS", "3DS"),
    ("ARCADE", "Arcade", "Arcade"),
    ("DC", "Dreamcast", "Dreamcast"),
    ("FBN", "Final Burn Neo", "FBN"),
    ("GBA", "Game Boy Advance", "GBA"),
    ("GCN", "Nintendo GameCube", "GC"),
    ("GENESIS", "Sega Genesis", "Genesis"),
    ("N64", "Nintendo 64", "N64"),
    ("NES", "Nintendo Entertainment System", "NES"),
    ("PS2", "PlayStation 2", "PS2"),
    ("PS3", "PlayStation 3", "PS3"),
    ("PS4", "PlayStation 4", "PS4"),
    ("PSP", "PlayStation Portable", "PSP"),
    ("PSX", "PlayStation", "PSX"),
    ("SNES", "Super Nintendo", "SNES"),
    ("SATURN", "Sega Saturn", "Saturn"),
    ("SWITCH", "Nintendo Switch", "Switch"),
    ("WII", "Nintendo Wii", "Wii"),
    ("WIIU", "Nintendo Wii U", "WiiU"),
    ("X360", "Xbox 360", "X360"),
    ("XBOX", "Xbox", "Xbox"),
];

/// 384px thumbnails of Lacrima's Assets/Consoles art, embedded so the
/// exe stays self-contained. Extracted to the data dir at startup and
/// referenced as `CoverSrc::File` — the row's decode/cache path then
/// handles them like any folder cover.
const ART: &[(&str, &[u8])] = &[
    ("3DS", include_bytes!("../../assets/consoles/3DS.png")),
    ("Arcade", include_bytes!("../../assets/consoles/Arcade.png")),
    (
        "Dreamcast",
        include_bytes!("../../assets/consoles/Dreamcast.png"),
    ),
    ("FBN", include_bytes!("../../assets/consoles/FBN.png")),
    ("GBA", include_bytes!("../../assets/consoles/GBA.png")),
    ("GC", include_bytes!("../../assets/consoles/GC.png")),
    ("Genesis", include_bytes!("../../assets/consoles/Genesis.png")),
    ("N64", include_bytes!("../../assets/consoles/N64.png")),
    ("NES", include_bytes!("../../assets/consoles/NES.png")),
    ("PS2", include_bytes!("../../assets/consoles/PS2.png")),
    ("PS3", include_bytes!("../../assets/consoles/PS3.png")),
    ("PS4", include_bytes!("../../assets/consoles/PS4.png")),
    ("PSP", include_bytes!("../../assets/consoles/PSP.png")),
    ("PSX", include_bytes!("../../assets/consoles/PSX.png")),
    ("SNES", include_bytes!("../../assets/consoles/SNES.png")),
    ("Saturn", include_bytes!("../../assets/consoles/Saturn.png")),
    ("Switch", include_bytes!("../../assets/consoles/Switch.png")),
    ("Wii", include_bytes!("../../assets/consoles/Wii.png")),
    ("WiiU", include_bytes!("../../assets/consoles/WiiU.png")),
    ("X360", include_bytes!("../../assets/consoles/X360.png")),
    ("Xbox", include_bytes!("../../assets/consoles/Xbox.png")),
];

/// Extract the embedded console art into `<data>/Assets/Consoles/`
/// (only missing/stale files are rewritten) — returns the dir.
/// Right-cluster slot packing for the emulation control band: `rx`
/// walks left from the band's right edge in 110s steps. A slot whose
/// 60s button would reach into the centered pause button's zone — or
/// land on an already-placed icon (`taken` centers) — parks at
/// `left_next` instead: the gap between the left toggles and the
/// pause, while room remains. `force_park` unconditionally parks the
/// slot (the balance split picks the innermost icons this way).
/// Returns NaN for absent slots (`cond == false`); keeps the packed
/// position when no spot is free (degenerate ultra-narrow fallback).
fn band_slot(
    cond: bool,
    force_park: bool,
    rx: &mut f32,
    left_next: &mut f32,
    taken: &mut Vec<f32>,
    pause: Rect,
    s: f32,
) -> f32 {
    if !cond {
        return f32::NAN;
    }
    let mut x = *rx;
    *rx -= 110.0 * s;
    let gap = 10.0 * s;
    let blocked = |p: f32| taken.iter().any(|&t| (p - t).abs() < 70.0 * s);
    let intrudes = x - 30.0 * s < pause.max.x + gap && x + 30.0 * s > pause.min.x - gap;
    if force_park || intrudes || blocked(x) {
        let cand = *left_next;
        if cand + 30.0 * s <= pause.min.x - gap && !blocked(cand) {
            x = cand;
            *left_next = cand + 110.0 * s;
        }
    }
    taken.push(x);
    x
}

/// Where a carousel Play action goes (Lacrima's PlayCommand →
/// RequestEmulatorLaunch shape): the running game only un-dismisses
/// the capture viewport; a different game while a session runs
/// queues a pending launch and closes the emulator first; no
/// session launches straight away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayRoute {
    UnDismiss,
    QueuePending,
    Launch,
}

fn play_route(session_on: bool, running_same: bool) -> PlayRoute {
    if running_same {
        PlayRoute::UnDismiss
    } else if session_on {
        PlayRoute::QueuePending
    } else {
        PlayRoute::Launch
    }
}

/// Balance split for the emulation band: returns the present-icon
/// ordinal at which slots should start parking left, so the band
/// reads symmetric around the centered pause (`n_left` icons already
/// occupy the left side). The right side keeps `ceil(total/2)` icons
/// when the room allows it, bounded by `right_cap`; the overflow —
/// at most `left_cap` — parks left.
fn band_spill_from(n_left: usize, n_right: usize, right_cap: usize, left_cap: usize) -> usize {
    let right_target = ((n_left + n_right + 1) / 2).min(right_cap).min(n_right);
    n_right - (n_right - right_target).min(left_cap)
}

fn extract_art() -> Option<PathBuf> {
    let dir = settings::data_root()?.join("Assets").join("Consoles");
    std::fs::create_dir_all(&dir).ok()?;
    for (name, bytes) in ART {
        let path = dir.join(format!("{name}.png"));
        let stale = std::fs::metadata(&path)
            .map(|m| m.len() != bytes.len() as u64)
            .unwrap_or(true);
        if stale {
            let _ = std::fs::write(&path, bytes);
        }
    }
    Some(dir)
}

/// A running settings-card download. `cores` marks a Libretro core
/// pack (the `done` result is a summary and the section's core list
/// refreshes); `None` means a handler-exe install (the result is a
/// path written to `emulators[handler]`).
pub struct DlJob {
    /// `spec.handler` — the `cfg.emulators` key for exe installs and
    /// the `installed_hint` key to invalidate.
    pub handler: &'static str,
    /// `spec.name` — the display name for the "installed" line.
    pub name: &'static str,
    /// Libretro core-pack section, when this is a core download.
    pub cores: Option<libretro::Section>,
    /// The shared progress handle the worker thread updates.
    pub shared: emu_dl::Shared,
}

/// Per-view state for Emulation.
pub struct EmuView {
    /// The console-section strip — Lacrima's EmulationListView shares
    /// the album-row control, so does ours.
    pub row: album_row::AlbumRow,
    /// Section tiles: one synthesized `Album` per console; `dir` is a
    /// synthetic `emu:` key and `covers` the extracted console art.
    pub sections: Vec<library::Album>,
    /// Section-strip visibility — Lacrima's IsAlbumListCollapsed
    /// (session-only, not persisted).
    pub list_open: bool,
    /// The item search pill (RomSearch) — filtering lands with items.
    pub search: search_pill::SearchPill,
    /// The selected section index — drives the item-area header.
    pub active: Option<usize>,
    /// Settings tab: which console cards are expanded (IsExpanded —
    /// session-only like Lacrima's view-model state).
    pub expanded: HashSet<String>,
    /// The item list — the same `Carousel` the audio view drives
    /// (carousel default, cover-layout modes, search filter,
    /// hold-to-lift reorder), in ROM mode (Add Roms / Scan Folder /
    /// Remove / Clear Section menus).
    pub cards: carousel::Carousel,
    /// Games of the active section — the persisted list the cards
    /// view renders; `sections[active].tracks` mirrors it.
    pub items: Vec<PathBuf>,
    /// Which section key `items` belongs to.
    pub loaded_key: Option<String>,
    /// In-flight metadata/cover resolver for the loaded section —
    /// titles + downloaded covers stream back as they resolve.
    /// In-flight cover/title resolvers — each paired with a cancel
    /// flag so Clear Section Cache can stop the workers' sidecar
    /// writes, not just drop their results (Lacrima's
    /// CancelAllAlbumCoverScans).
    pub meta_rxs: Vec<(
        mpsc::Receiver<emu_meta::RomInfo>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    )>,
    /// Card-menu/Delete-key removals waiting on the shared delete
    /// confirmation (main.rs routes these through the dialog).
    pub pending_delete: Option<Vec<PathBuf>>,
    /// Running emulator session (Xenia launch + WGC capture).
    pub session: Option<xenia::Session>,
    /// IsEmulatorViewportDismissed — the user switched back to the
    /// card list while the game keeps running underneath.
    pub viewport_dismissed: bool,
    /// Lacrima's IsFullscreen — the game fills the whole SCREEN: the
    /// app window goes to real OS fullscreen and every piece of
    /// chrome (band, strip, search, cards) hides. Esc/double-click
    /// exits and restores the previous windowed state.
    pub game_fs: bool,
    /// Whether WE put the window into fullscreen for `game_fs` —
    /// leaving restores the prior windowed state only in that case.
    fs_drove: bool,
    /// The Fullscreen(true) request actually landed (win_fs observed
    /// true) — distinguishes "in flight" from "user exited".
    fs_seen: bool,
    /// Maximized flag captured on entry — restored on exit.
    fs_prev_max: bool,
    /// The OS cursor is hidden while the capture owns the screen.
    cursor_hidden: bool,
    /// Presentation-rect tween state — `pres_anim` tweens from
    /// `pres_from` toward the live target over a fixed duration, but
    /// ONLY when `pres_fs` observes a fullscreen transition; any
    /// other target motion (the strip's eased slide, resizes) is
    /// followed 1:1 so a frame hitch can't be misread as a retarget.
    pres_anim: Rect,
    pres_from: Rect,
    pres_t: f32,
    pres_fs: bool,
    /// Aspect-fit ↔ aspect-fill blend — a fixed 0.55s cubic tween
    /// (same profile/duration as the strip slide) so every moving
    /// part shares one choreography instead of fighting.
    crop_t: f32,
    crop_from: f32,
    crop_target: f32,
    crop_prog: f32,
    /// Debug: records the capture rect trajectory while the strip
    /// slide or fs tween runs → %LOCALAPPDATA%\AES_Rustified\motion.log
    motion_log: Option<std::fs::File>,
    motion_t0: std::time::Instant,
    /// CapturePresentationOpacity — eased 0..1 fade of the opaque
    /// session layer over the carousel (0.3s like Lacrima's
    /// DoubleTransition).
    pub cap_t: f32,
    /// egui texture captured frames upload into.
    pub frame_tex: Option<egui::TextureHandle>,
    /// Back-cover letterbox fill (EmulationUseBackCoverLetterboxFill):
    /// the game whose "Back Cover" `.meta` payload `letterbox_tex`
    /// holds — kept through the fade-out after the session ends.
    letterbox_key: Option<PathBuf>,
    letterbox_tex: Option<egui::TextureHandle>,
    /// Last frame seq uploaded into `frame_tex`.
    pub frame_seq: u64,
    /// Persistent frame staging buffer — swapped with the capture
    /// slot so neither side allocates per frame.
    frame_buf: Vec<u8>,
    /// One-line notice for the item area (launch errors, hints).
    pub notice: Option<String>,
    /// In-flight emulator downloads (handler install or Libretro
    /// core pack), keyed by the OWNING card's section ("GBA") — the
    /// progress bar renders only there and its button disables only
    /// there; other cards can still start their own downloads. A
    /// finished job is released by whichever card renders first —
    /// the job carries the handler/name/section so the result lands
    /// in the right slot regardless.
    pub dl: HashMap<String, DlJob>,
    /// Last download result line per card (owner key → message).
    pub dl_msg: HashMap<String, String>,
    /// Cached `spec.installed_exe` results per handler — the
    /// resolvers walk the whole managed install (RetroArch alone is
    /// ~10k files) and this ran once per expanded card per frame,
    /// which is what made the settings tab crawl during/after a
    /// download.
    installed_hint: HashMap<String, (std::time::Instant, Option<PathBuf>)>,
    /// Cached Libretro core list per section — catalog read +
    /// per-core file stats, refreshed at most once a second instead
    /// of every frame. A single-slot cache thrashed between expanded
    /// RetroArch cards (each frame: key mismatch → filesystem scan).
    cores_hint: HashMap<String, (std::time::Instant, Vec<libretro::CoreInfo>)>,
    /// The ROM metadata editor overlay (EmulationMetadataOverlay).
    pub editor: rom_editor::RomEditor,
    /// Xenia per-game Patches overlay (XeniaPatchesOverlay).
    pub patches: xenia_overlay::PatchesOverlay,
    /// Xenia per-game Custom Config overlay (XeniaCustomConfigOverlay).
    pub cfg_ov: xenia_overlay::ConfigOverlay,
    /// RPCS3 per-game Patches overlay (Rpcs3PatchesOverlay).
    pub rpcs3_patches: rpcs3_overlay::Rpcs3PatchesOverlay,
    /// shadPS4 per-game Patches overlay (ShadPs4PatchesOverlay).
    pub shad_patches: shadps4_overlay::ShadPs4PatchesOverlay,
    /// shadPS4 per-game Cheats overlay (ShadPs4CheatsOverlay).
    pub shad_cheats: shadps4_overlay::ShadPs4CheatsOverlay,
    /// Cemu per-game Graphic Packs overlay (CemuGraphicPacksOverlay).
    pub cemu_packs: cemu_overlay::CemuGraphicPacksOverlay,
    /// Xenia profile-creation prompt — opens when the X360 section is
    /// entered and no profile exists under the Xenia content dir.
    pub prof_prompt: xenia_overlay::ProfilePrompt,
    /// Set by open_section when the prompt should fire once `view()`
    /// runs (open needs a Context for the fade seed).
    prof_want: bool,
    /// Section-entry update check — spawned the first time a section
    /// with a managed handler is entered (section key + shared
    /// result), polled in `view()`.
    upd_check: Option<(String, emu_dl::VerShared)>,
    /// Handler ids already checked this run — one check per handler
    /// so the shared RetroArch/Dolphin installs don't prompt from
    /// every backed section or spawn duplicate downloads.
    upd_checked: HashSet<String>,
    /// Sections whose update prompt was skipped — the band shows an
    /// update icon while one is active so it stays reachable. Maps
    /// the section key to the subtitle its prompt showed.
    upd_skipped: HashMap<String, String>,
    /// The "update available" prompt — same confirm dialog the
    /// fullscreen-exit and delete prompts use.
    upd_open: bool,
    /// Painted fade (trails `upd_open`) + focused button — mirrors
    /// main.rs's FsExitPrompt fields.
    upd_vis: f32,
    upd_sel: usize,
    /// Dialog text for the open prompt — the section key it belongs
    /// to, "installed → latest" for the subtitle.
    upd_key: String,
    upd_subtitle: String,
    /// The running update job — `Some` turns the prompt into its
    /// progress/status view until the worker reports done.
    upd_job: Option<emu_dl::Shared>,
    /// Launch pipeline — config prep + Session::start run on a
    /// worker; the result lands here (Lacrima's LaunchEmulatorAsync
    /// Task.Run shape so template downloads never stall the UI).
    pub launch_rx: Option<mpsc::Receiver<Result<xenia::Session, String>>>,
    /// ScanFolder — the directory walk runs on a worker (big trees
    /// stall the UI otherwise); found ROMs land here.
    pub scan_rx: Option<mpsc::Receiver<Vec<PathBuf>>>,
    /// Set when `cfg.emulators`/`cfg.games`/`cfg.section_order`/
    /// `cfg.default_handlers` changed — main persists.
    pub settings_dirty: bool,
    /// Capture render-options panel open (Lacrima's
    /// RenderOptionsOverlay) — right-side slide panel.
    pub render_open: bool,
    /// The loaded section's live render options — written through to
    /// `cfg.render` and pushed into the session's RenderShared.
    pub render: settings::RenderOpts,
    /// Set when `render` changed so view() writes it into cfg.
    render_dirty: bool,
    /// Extracted handler-exe icon for the setup button — keyed by
    /// the resolved exe path so a handler switch reloads it.
    handler_icon_key: Option<String>,
    handler_icon: Option<egui::TextureHandle>,
    /// Resolved handler exe for the setup button — cached on the
    /// "handler|configured-path" pair so the band doesn't stat the
    /// filesystem every frame.
    setup_exe_key: String,
    setup_exe: Option<PathBuf>,
    /// Stats-overlay state — capture fps/frametime tracking and the
    /// rolling frametime ring for the graph.
    stats_instant: Option<std::time::Instant>,
    stats_fps: f32,
    stats_ft_ms: f32,
    frametimes: std::collections::VecDeque<f32>,
    gpu_desc: Option<String>,
    /// Extracted console-art dir — the strip tiles' fallback cover.
    art_dir: Option<PathBuf>,
    /// Last-seen `use_first_item_cover` — a settings flip re-themes
    /// every section tile (first ROM cover ⇄ console art).
    prev_first_cover: bool,
    /// GPU capture-shader pipelines (Lacrima's Shader file ComboBox)
    /// — `None` when the egui backend isn't wgpu.
    pub cap_shader: Option<crate::emulation::cap_shader::CaptureShader>,
    /// The imported zero-copy ring — frames are sampled straight from
    /// the D3D11 shared textures (Lacrima's GPU interop); `None` puts
    /// us on the CPU-mirror path (same as Lacrima's fallback).
    gpu_cap: Option<crate::emulation::cap_shader::ImportedRing>,
    /// Generation whose import already failed — don't retry every
    /// frame; a new generation (size change) gets a fresh attempt.
    gpu_failed_gen: u64,
    /// Why the last import failed — surfaced in the stats overlay
    /// ("CPU capture (…)") instead of silently falling back.
    gpu_err: Option<String>,
    /// Last CPU frame-slot seq consumed — separate from `frame_seq`,
    /// which tracks the GPU ring on the zero-copy path.
    slot_seq: u64,
    /// Pillarbox auto-crop (Remove side pillars) — measured bar
    /// insets in frame px. `cur` tweens toward `tgt`; `tgt` only
    /// moves after `stable` consecutive identical detections
    /// (Lacrima's 6-frame debounce).
    pillar_cur: (f32, f32),
    pillar_tgt: (u32, u32),
    pillar_last: (u32, u32),
    pillar_stable: u32,
    pillar_from: (f32, f32),
    pillar_prog: f32,
    /// User-locked crop from the ROM's `.meta` — wins over detection
    /// (Lacrima's ArcadePillarboxCropResolver.IsLocked).
    pillar_lock: Option<(u32, u32, u32)>,
    /// "Reboot" context-menu request — relaunches the running game at
    /// the view level where `cfg` is in scope.
    reboot: bool,
    /// A UI close request (power button, "Close Emulator" menu item)
    /// parked for the caller — it owns the confirm prompt that guards
    /// every session stop.
    close_req: bool,
    /// Lacrima's `_pendingEmulatorLaunchRequest` — a double-click on
    /// another game while a session runs stores the request here and
    /// closes the running emulator; it fires once the old session is
    /// fully gone (TryLaunchPendingEmulatorRequest).
    pending_launch: Option<PathBuf>,
    /// Whether the saved aspect_key was applied to this session's
    /// window (the HWND resolves asynchronously after launch).
    aspect_applied: bool,
    /// Input tunnel — pointer/keys over the capture forward to the
    /// hidden emulator window via PostMessage so its own UI (Xenia's
    /// sign-in prompts etc.) is reachable. Session-only; Esc or the
    /// band button releases it.
    pub input_tun: bool,
    /// The live frame mapping the tunnel needs — `(dest, uv)` —
    /// refreshed every frame the session draw path computes them.
    tun_map: Option<(Rect, Rect)>,
    /// Held mouse-button mask (MK_* bits on Windows, BTN_* bits off
    /// it) + tracked modifier state so tunneled events carry correct
    /// key flags.
    tun_buttons: u8,
    tun_mods: u8,
    /// Diagnostics — forwarded-event count + last post result, shown
    /// in the stats overlay so tunnel issues are visible instead of
    /// silently dead.
    pub tun_sent: u32,
    pub tun_dbg: Option<String>,
}

impl EmuView {
    pub fn new() -> Self {
        let art_dir = extract_art();
        let sections = CONSOLES
            .iter()
            .map(|(key, title, art)| library::Album {
                title: (*title).to_string(),
                dir: PathBuf::from(format!("emu:{key}")),
                tracks: Vec::new(),
                covers: if art.is_empty() {
                    Vec::new()
                } else {
                    art_dir
                        .as_ref()
                        .map(|d| vec![library::CoverSrc::File(d.join(format!("{art}.png")))])
                        .unwrap_or_default()
                },
                url_count: 0,
                added: Vec::new(),
            })
            .collect();
        Self {
            row: album_row::AlbumRow::new(),
            sections,
            list_open: true,
            search: search_pill::SearchPill::default(),
            active: None,
            expanded: HashSet::new(),
            cards: {
                let mut c = carousel::Carousel::new();
                c.rom_mode = true;
                c
            },
            items: Vec::new(),
            loaded_key: None,
            meta_rxs: Vec::new(),
            pending_delete: None,
            session: None,
            viewport_dismissed: false,
            game_fs: false,
            fs_drove: false,
            fs_seen: false,
            fs_prev_max: false,
            cursor_hidden: false,
            pres_anim: Rect::NOTHING,
            pres_from: Rect::NOTHING,
            pres_t: 1.0,
            pres_fs: false,
            crop_t: 0.0,
            crop_from: 0.0,
            crop_target: 0.0,
            crop_prog: 1.0,
            motion_log: None,
            motion_t0: std::time::Instant::now(),
            cap_t: 0.0,
            frame_tex: None,
            letterbox_key: None,
            letterbox_tex: None,
            frame_seq: 0,
            frame_buf: Vec::new(),
            notice: None,
            dl: HashMap::new(),
            dl_msg: HashMap::new(),
            installed_hint: HashMap::new(),
            cores_hint: HashMap::new(),
            editor: rom_editor::RomEditor::new(),
            patches: xenia_overlay::PatchesOverlay::new(),
            cfg_ov: xenia_overlay::ConfigOverlay::new(),
            rpcs3_patches: rpcs3_overlay::Rpcs3PatchesOverlay::new(),
            shad_patches: shadps4_overlay::ShadPs4PatchesOverlay::new(),
            shad_cheats: shadps4_overlay::ShadPs4CheatsOverlay::new(),
            cemu_packs: cemu_overlay::CemuGraphicPacksOverlay::new(),
            prof_prompt: xenia_overlay::ProfilePrompt::new(),
            prof_want: false,
            upd_check: None,
            upd_checked: HashSet::new(),
            upd_skipped: HashMap::new(),
            upd_open: false,
            upd_vis: 0.0,
            upd_sel: 0,
            upd_key: String::new(),
            upd_subtitle: String::new(),
            upd_job: None,
            launch_rx: None,
            scan_rx: None,
            settings_dirty: false,
            render_open: false,
            render: settings::RenderOpts::default(),
            render_dirty: false,
            handler_icon_key: None,
            handler_icon: None,
            setup_exe_key: String::new(),
            setup_exe: None,
            stats_instant: None,
            stats_fps: 0.0,
            stats_ft_ms: 0.0,
            frametimes: std::collections::VecDeque::new(),
            gpu_desc: None,
            art_dir,
            prev_first_cover: false,
            cap_shader: None,
            gpu_cap: None,
            gpu_failed_gen: 0,
            gpu_err: None,
            slot_seq: 0,
            pillar_cur: (0.0, 0.0),
            pillar_tgt: (0, 0),
            pillar_last: (0, 0),
            pillar_stable: 0,
            pillar_from: (0.0, 0.0),
            pillar_prog: 1.0,
            pillar_lock: None,
            reboot: false,
            close_req: false,
            pending_launch: None,
            aspect_applied: false,
            input_tun: false,
            tun_map: None,
            tun_buttons: 0,
            tun_mods: 0,
            tun_sent: 0,
            tun_dbg: None,
        }
    }

    /// Lacrima's EmulationUseFirstItemCover — when on, a section tile
    /// wears its first ROM's box art (console art when that ROM has no
    /// cover yet); when off, tiles always show console art.
    pub fn refresh_section_covers(&mut self, cfg: &settings::Emulation) {
        let art_dir = self.art_dir.clone();
        for sec in &mut self.sections {
            let key = section_key(sec);
            let want = if cfg.use_first_item_cover {
                sec.tracks
                    .first()
                    .and_then(|p| rom_cover(p))
                    .map(|c| vec![c])
                    .unwrap_or_else(|| console_art(art_dir.as_deref(), &key))
            } else {
                console_art(art_dir.as_deref(), &key)
            };
            if sec.covers != want {
                sec.covers = want;
            }
        }
    }

    /// Load persisted game lists into the section tiles and apply
    /// the saved strip order (called once at startup — the strip
    /// counts read `tracks.len()`).
    pub fn seed_games(&mut self, cfg: &settings::Emulation) {
        for sec in &mut self.sections {
            sec.tracks = cfg
                .games
                .get(&section_key(sec))
                .map(|g| g.iter().map(PathBuf::from).collect())
                .unwrap_or_default();
        }
        // Lacrima's "AlbumOrder": saved drag order first, catalog
        // order appended for sections the save doesn't know.
        let rank: HashMap<&str, usize> = cfg
            .section_order
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i))
            .collect();
        self.sections.sort_by_key(|s| {
            rank.get(section_key(s).as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
        self.refresh_section_covers(cfg);
    }

    /// Persist the active section's game list back into settings and
    /// the section tile; marks `settings_dirty`.
    fn sync_games(&mut self, cfg: &mut settings::Emulation) {
        if let Some(key) = self.loaded_key.clone() {
            let list: Vec<String> = self
                .items
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            cfg.games.insert(key.clone(), list);
            if let Some(sec) = self.sections.iter_mut().find(|s| section_key(s) == key) {
                sec.tracks = self.items.clone();
            }
            self.settings_dirty = true;
        }
        self.refresh_section_covers(cfg);
    }

    /// The active section's console art — the cards' fallback cover
    /// (always console art even when tiles wear the first-ROM cover).
    fn fallback_cover(&self) -> Option<library::CoverSrc> {
        let key = self
            .active
            .and_then(|i| self.sections.get(i))
            .map(|s| section_key(s))?;
        console_art(self.art_dir.as_deref(), &key)
            .into_iter()
            .next()
    }

    /// Select + load a section — the shared body of the strip's
    /// double-click and keyboard Enter paths. Switching sections while
    /// a game is being captured means the user wants the list: the
    /// presentation is dismissed so the newly opened section shows
    /// (the game keeps running underneath — same side effects as the
    /// band's viewport toggle).
    pub fn activate_section(&mut self, i: usize, cfg: &settings::Emulation) {
        let switching = self.active != Some(i);
        self.active = Some(i);
        self.open_section(i, cfg);
        if switching && self.session.is_some() && !self.viewport_dismissed {
            self.dismiss_viewport();
        }
    }

    /// Dismiss the capture viewport — the ⇄ band button's hide
    /// half: the card list comes back, so fullscreen capture and
    /// the input tunnel no longer have anything to drive.
    pub fn dismiss_viewport(&mut self) {
        self.viewport_dismissed = true;
        self.game_fs = false;
        self.input_tun = false;
    }

    /// The band's ⇄ switch — capture ↔ card list, shared by the
    /// band button and Ctrl+S.
    pub fn toggle_viewport(&mut self) {
        if self.viewport_dismissed {
            self.viewport_dismissed = false;
        } else {
            self.dismiss_viewport();
        }
    }

    /// Capture fullscreen — the capture's double-click, the session
    /// context-menu "Fullscreen" entry and Ctrl+E all share this.
    /// Entering also un-dismisses the viewport: `game_fs` alone is a
    /// no-op while the capture isn't on screen, and asking for
    /// fullscreen is itself the request to be there.
    pub fn toggle_fullscreen(&mut self) {
        self.game_fs = !self.game_fs;
        if self.game_fs {
            self.viewport_dismissed = false;
        }
    }

    /// The input-tunnel toggle — the band's pointer icon and Ctrl+M:
    /// while lit, clicks/keys over the capture PostMessage into the
    /// emulator's hidden window.
    pub fn toggle_input_tunnel(&mut self) {
        self.input_tun = !self.input_tun;
    }

    /// Play a card by path — the carousel's `Action::Play` routing
    /// as a method so keyboard Enter shares it: the running game
    /// just un-dismisses the viewport, a different game queues
    /// behind the running session's close, no session launches
    /// straight away.
    pub fn play_card(&mut self, game: &Path, cfg: &settings::Emulation) {
        let running_same = self.session.as_ref().is_some_and(|s| s.game == game);
        match play_route(self.session.is_some(), running_same) {
            PlayRoute::UnDismiss => self.viewport_dismissed = false,
            PlayRoute::QueuePending => {
                self.pending_launch = Some(game.to_path_buf());
                self.stop_session();
            }
            PlayRoute::Launch => self.launch(cfg, game),
        }
    }

    /// Load a section's persisted games into the card view and spawn
    /// the metadata/cover resolver (idempotent on re-select).
    pub fn open_section(&mut self, i: usize, cfg: &settings::Emulation) {
        let key = section_key(&self.sections[i]);
        if self.loaded_key.as_deref() == Some(key.as_str()) {
            return;
        }
        self.loaded_key = Some(key.clone());
        // Xenia can't show its own "create a profile" dialog through
        // the hidden capture window — if no profile exists on disk,
        // surface our own prompt once per section entry. (Computed
        // before `key` moves into the resolver below.)
        self.prof_want = key == XENIA_SECTION
            && self.active_handler(cfg).as_deref() == Some(XENIA_HANDLER)
            && !self.prof_prompt.is_open()
            && xenia_storage(cfg).is_some_and(|r| xenia_profile::find_xuids(&r).is_empty());
        // First entry this run: with the section's handler installed,
        // check for an upstream update in the background — `view()`
        // opens the prompt if the versions differ. Keyed by handler
        // (RetroArch covers nine sections, Dolphin two) so a shared
        // install only prompts once and can't queue parallel
        // downloads of itself; skipped while a check is in flight so
        // its result can't be silently overwritten.
        if self.upd_check.is_none() {
            if let Some(spec) = handler_spec(&key) {
                let installed = cfg
                    .emulators
                    .get(spec.handler)
                    .is_some_and(|p| !p.trim().is_empty())
                    || (spec.installed_exe)().is_some();
                if installed && self.upd_checked.insert(spec.handler.to_string()) {
                    self.upd_check =
                        Some((key.clone(), emu_dl::update_check(spec.handler)));
                }
            }
        }
        // Per-section render options (Lacrima's SectionSettings) —
        // the panel edits this live copy.
        self.render = cfg.render.get(&key).cloned().unwrap_or_default();
        self.items = cfg
            .games
            .get(&key)
            .map(|g| g.iter().map(PathBuf::from).collect())
            .unwrap_or_default();
        self.sections[i].tracks = self.items.clone();
        self.cards.open_roms(
            &self.sections[i],
            &rom_cover,
            console_art(self.art_dir.as_deref(), &key)
                .into_iter()
                .next(),
        );
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.meta_rxs.push((
            emu_meta::spawn_resolver(
                key,
                self.sections[i].title.clone(),
                self.items.clone(),
                cancel.clone(),
            ),
            cancel,
        ));
        self.refresh_section_covers(cfg);
        self.notice = None;
    }

    /// Resolved titles + fetched covers from the worker — applied to
    /// the card items as they stream in.
    fn drain_meta(&mut self, cfg: &settings::Emulation, ctx: &egui::Context) {
        let mut i = 0;
        let mut covers_landed = false;
        while i < self.meta_rxs.len() {
            let mut closed = false;
            loop {
                match self.meta_rxs[i].0.try_recv() {
                    Ok(info) => {
                        if let Some(title) = info.title {
                            self.cards.apply_info(
                                &info.path,
                                &library::TrackInfo {
                                    title,
                                    artist: String::new(),
                                    album: String::new(),
                                    number: None,
                                    cover_url: None,
                                    duration: None,
                                },
                            );
                        }
                        if let Some(cov) = info.cover {
                            self.cards
                                .set_cover(&info.path, library::CoverSrc::File(cov));
                            covers_landed = true;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                self.meta_rxs.remove(i);
            } else {
                i += 1;
            }
        }
        // A landed cover may be a section tile's new face.
        if covers_landed {
            self.refresh_section_covers(cfg);
        }
        // Resolvers still in flight — keep repainting so results
        // land without waiting on input events.
        if !self.meta_rxs.is_empty() {
            ctx.request_repaint();
        }
    }

    /// Add rom paths to the active section (deduped through the card
    /// view so order/selection stay consistent), then persist.
    pub fn add_games(&mut self, cfg: &mut settings::Emulation, paths: Vec<PathBuf>) {
        let fallback = self.fallback_cover();
        let mut added = Vec::new();
        for p in paths {
            if self.cards.add_rom(p.clone(), &rom_cover, &fallback) {
                self.items.push(p.clone());
                added.push(p);
            }
        }
        if !added.is_empty() {
            let key = self.loaded_key.clone().unwrap_or_default();
            let platform = self
                .sections
                .iter()
                .find(|c| section_key(c) == key)
                .map(|c| c.title.clone())
                .unwrap_or_else(|| key.clone());
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.meta_rxs.push((
                emu_meta::spawn_resolver(key, platform, added, cancel.clone()),
                cancel,
            ));
            self.sync_games(cfg);
        }
    }

    /// Item-order mirror after a card drag-reorder landed.
    pub fn sync_order(&mut self, cfg: &mut settings::Emulation) {
        self.items = self.cards.file_items();
        self.sync_games(cfg);
    }

    pub fn remove_games(&mut self, cfg: &mut settings::Emulation, paths: &[PathBuf]) {
        self.cards.remove_many(paths);
        self.items = self.cards.file_items();
        self.sync_games(cfg);
    }

    pub fn clear_games(&mut self, cfg: &mut settings::Emulation) {
        self.cards.clear_items();
        self.items.clear();
        self.sync_games(cfg);
    }

    /// ClearAlbumCache — delete the fetch sidecars (`.img` cover +
    /// `.title` name) for every loaded rom, reset the cards to their
    /// fallback art, and re-run the resolver so titles/covers rebuild
    /// from scratch (Lacrima's ClearAlbumCacheCommand). User data —
    /// the `.meta` doc + editor images — is preserved.
    pub fn clear_section_cache(&mut self) {
        let Some(key) = self.loaded_key.clone() else {
            return;
        };
        if self.items.is_empty() {
            return;
        }
        // Stale in-flight results would re-apply covers we just
        // deleted — drop the receivers AND cancel the workers so their
        // sidecar writes can't race the fresh refetch (Lacrima's
        // CancelAllAlbumCoverScans).
        for (_, c) in &self.meta_rxs {
            c.store(true, Ordering::Relaxed);
        }
        self.meta_rxs.clear();
        for p in &self.items {
            emu_meta::clear_cache(p);
        }
        if let Some(i) = self.sections.iter().position(|c| section_key(c) == key) {
            // Reload with no cached covers — cards show the fallback
            // art until the resolver streams fresh results in.
            self.cards.open_roms(
                &self.sections[i],
                &rom_cover,
                console_art(self.art_dir.as_deref(), &key)
                    .into_iter()
                    .next(),
            );
        }
        let platform = self
            .sections
            .iter()
            .find(|c| section_key(c) == key)
            .map(|c| c.title.clone())
            .unwrap_or_else(|| key.clone());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.meta_rxs.push((
            emu_meta::spawn_resolver(key, platform, self.items.clone(), cancel.clone()),
            cancel,
        ));
        self.notice = Some("Section cache cleared — refetching…".into());
    }

    /// The handler id that launches the loaded section — Lacrima's
    /// `SelectedHandlerId` (the section's default handler), falling
    /// back to the one wired handler for X360.
    pub fn active_handler(&self, cfg: &settings::Emulation) -> Option<String> {
        let key = self.loaded_key.as_deref()?;
        if let Some(h) = cfg.default_handlers.get(key) {
            return Some(h.clone());
        }
        match key {
            XENIA_SECTION => Some(XENIA_HANDLER.to_string()),
            DC_SECTION => Some(FLYCAST_HANDLER.to_string()),
            XBOX_SECTION => Some(XEMU_HANDLER.to_string()),
            PS3_SECTION => Some(RPCS3_HANDLER.to_string()),
            PS4_SECTION => Some(SHADPS4_HANDLER.to_string()),
            PSX_SECTION => Some(DUCKSTATION_HANDLER.to_string()),
            PS2_SECTION => Some(PCSX2_HANDLER.to_string()),
            GCN_SECTION | WII_SECTION => Some(DOLPHIN_HANDLER.to_string()),
            WIIU_SECTION => Some(CEMU_HANDLER.to_string()),
            SWITCH_SECTION => Some(EDEN_HANDLER.to_string()),
            THREE_DS_SECTION
            | N64_SECTION
            | NES_SECTION
            | GENESIS_SECTION
            | SNES_SECTION
            | GBA_SECTION
            | PSP_SECTION
            | SATURN_SECTION
            | FBN_SECTION
            | ARCADE_SECTION => Some(RETROARCH_HANDLER.to_string()),
            _ => None,
        }
    }

    /// Resolve the section handler's executable — the configured
    /// path when it exists, else the managed install. Shared by
    /// game launches and the handler-setup button. The result is
    /// cached on the handler-id + configured-path pair (Lacrima's
    /// `CanLaunchCurrentSectionHandlerSetup` re-query shape).
    fn setup_exe(&mut self, cfg: &settings::Emulation) -> Option<PathBuf> {
        let handler = self.active_handler(cfg)?;
        let configured = cfg.emulators.get(&handler).cloned().unwrap_or_default();
        let key = format!("{handler}|{configured}");
        if key != self.setup_exe_key {
            self.setup_exe_key = key;
            let valid = |p: &str| !p.trim().is_empty() && Path::new(p.trim()).exists();
            self.setup_exe = if valid(&configured) {
                Some(PathBuf::from(configured.trim()))
            } else {
                match handler.as_str() {
                    XENIA_HANDLER => emu_dl::xenia_exe(),
                    FLYCAST_HANDLER => emu_dl::flycast_exe(),
                    XEMU_HANDLER => emu_dl::xemu_exe(),
                    RPCS3_HANDLER => emu_dl::rpcs3_exe(),
                    SHADPS4_HANDLER => emu_dl::shadps4_exe(),
                    DUCKSTATION_HANDLER => emu_dl::duckstation_exe(),
                    PCSX2_HANDLER => emu_dl::pcsx2_exe(),
                    DOLPHIN_HANDLER => emu_dl::dolphin_exe(),
                    CEMU_HANDLER => emu_dl::cemu_exe(),
                    EDEN_HANDLER => emu_dl::eden_exe(),
                    RETROARCH_HANDLER => emu_dl::retroarch_exe(),
                    _ => None,
                }
            };
        }
        self.setup_exe.clone()
    }

    /// Launch `game` through the section's default handler — resolves
    /// the render window, hides it, and starts WGC capture on a
    /// worker thread.
    pub fn launch(&mut self, cfg: &settings::Emulation, game: &Path) {
        self.notice = None;
        xenia::session_log(&format!("launch request game={}", game.display()));
        let handler = self.active_handler(cfg);
        let Some(kind) = handler_kind(handler.as_deref()) else {
            self.notice = Some("No emulator handler configured for this section".into());
            xenia::session_log(&format!("launch aborted: no handler (active={handler:?})"));
            return;
        };
        let name = match kind {
            xenia::Kind::Xenia => "Xenia",
            xenia::Kind::Flycast => "Flycast",
            xenia::Kind::Xemu => "xemu",
            xenia::Kind::Rpcs3 => "RPCS3",
            xenia::Kind::Shadps4 => "shadPS4",
            xenia::Kind::DuckStation => "DuckStation",
            xenia::Kind::Pcsx2 => "PCSX2",
            xenia::Kind::Dolphin => "Dolphin",
            xenia::Kind::Cemu => "Cemu",
            xenia::Kind::Eden => "Eden",
            xenia::Kind::RetroArch => "RetroArch",
        };
        let Some(exe) = self.setup_exe(cfg) else {
            self.notice = Some(format!(
                "No {name} executable — set a path or press Download in Settings → Emulation"
            ));
            xenia::session_log(&format!("launch aborted: no {name} exe"));
            return;
        };
        // A qtlauncher selection upgrades to the sibling core exe —
        // the launcher ignores `-g` (Lacrima's
        // ResolveShadPs4ExecutablePath).
        let exe = if kind == xenia::Kind::Shadps4 {
            shadps4::resolve_exe_path(&exe)
        } else {
            exe
        };
        // RetroArch needs the section's selected core for `-L` — the
        // combobox selection (or the first installed core) is the
        // default. The section decides WHICH Libretro set (3DS cores
        // for 3DS, N64 cores for N64).
        let retroarch_core = if kind == xenia::Kind::RetroArch {
            let Some(section) = self
                .loaded_key
                .as_deref()
                .and_then(libretro::Section::from_key)
            else {
                self.notice = Some("No Libretro core catalog for this section".into());
                xenia::session_log("launch aborted: section has no core catalog");
                return;
            };
            match libretro::resolve_core(section, selected_core_id(cfg, section))
                .map(|c| c.core_path)
            {
                Some(core) => {
                    // Lacrima's TryGetSaturnBiosValidationError — the
                    // saturn handler refuses to launch without BIOS in
                    // the frontend system dir (the core would spawn and
                    // instantly die on "content load failed" otherwise).
                    if section == libretro::Section::Saturn {
                        if let Some(e) = xenia::saturn_bios_error(&exe, &core) {
                            self.notice = Some(e);
                            xenia::session_log("launch aborted: Saturn BIOS missing");
                            return;
                        }
                    }
                    Some(core)
                }
                None => {
                    self.notice = Some(format!(
                        "No {} core installed — press Download all cores in Settings → Emulation",
                        section.label()
                    ));
                    xenia::session_log(&format!("launch aborted: no {} core", section.label()));
                    return;
                }
            }
        } else {
            None
        };
        // Per-section capture pool depth (Lacrima's LowLatencyCapture
        // → FramePoolSize 1 vs 2).
        let low_latency = self
            .loaded_key
            .as_deref()
            .and_then(|k| cfg.render.get(k))
            .map(|r| r.low_latency)
            .unwrap_or(true);
        // LaunchEmulatorAsync: PrepareConfigForLaunch merges the
        // game's custom config onto the template before boot —
        // on a worker since the first run may download it.
        let (tx, rx) = mpsc::channel();
        let g = game.to_path_buf();
        std::thread::spawn(move || {
            // Signed-in slot 0 — the profile we provisioned (or any
            // existing one) logs in without Xenia's NoProfileDialog,
            // which can't be clicked through the hidden capture
            // window.
            let mut xuid = None;
            if kind == xenia::Kind::Xenia {
                if let Some(root) = xenia_cfg::storage_root(exe.parent()) {
                    xenia_cfg::prepare_for_launch(&root, xenia_cfg::title_id(&g).as_deref());
                    xuid = xenia_profile::find_xuids(&root).first().copied();
                }
            }
            let _ = tx.send(xenia::Session::start(
                &exe.to_string_lossy(),
                &g,
                low_latency,
                xuid,
                kind,
                retroarch_core,
            ));
        });
        self.launch_rx = Some(rx);
        self.notice = Some(format!("Preparing {name}…"));
    }

    /// Ask the running session to close the emulator.
    pub fn stop_session(&mut self) {
        if let Some(s) = &self.session {
            s.stop();
        }
    }

    /// Flag a close request — the caller turns it into the Switch-style
    /// "Close emulator?" confirm instead of stopping directly.
    pub fn request_close(&mut self) {
        self.close_req = true;
    }

    /// Consume a parked close request — once, so a stale flag can't
    /// reopen the prompt after it's been answered.
    pub fn take_close_request(&mut self) -> bool {
        std::mem::replace(&mut self.close_req, false)
    }

    pub fn take_settings_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.settings_dirty, false)
    }
}

/// The Emulation view: item area, control band, section strip.
#[allow(clippy::too_many_arguments)]
pub fn view(
    ui: &mut egui::Ui,
    content: Rect,
    alpha: f32,
    interactive: bool,
    emu: &mut EmuView,
    cfg: &mut settings::Emulation,
    car_cfg: &settings::Carousel,
    radius: u8,
    s: f32,
) {
    let row_rect = crate::album_row_rect(content, s);
    // Section-strip collapse — Lacrima's MaxHeightToggleBehavior
    // (0.55s CubicEaseInOut), the same animation the music strip uses.
    let list_t = crate::ease_in_out_cubic(ui.ctx().animate_bool_with_time(
        ui.id().with("emu-sections-open"),
        emu.list_open,
        0.55,
    ));
    let band_top_at = |t: f32| {
        let mt = content.max.y - (content.max.y - row_rect.min.y) * t;
        (mt - crate::TRANSPORT_BAND_H * s).max(content.min.y)
    };
    let menu_top = content.max.y - (content.max.y - row_rect.min.y) * list_t;
    let band_top = band_top_at(list_t);
    let band = Rect::from_min_max(
        Pos2::new(content.min.x, band_top),
        Pos2::new(content.max.x, menu_top),
    );
    let menu_band = Rect::from_min_max(Pos2::new(content.min.x, menu_top), content.max);
    let upper = Rect::from_min_max(content.min, Pos2::new(content.max.x, band.min.y));
    // The item area at the slide's ENDPOINT — `upper` itself animates
    // with `list_t`, so the stable fit/fill decisions in the capture
    // layer must look at where the slide is heading, not the rect as
    // it eases past intermediate aspects (else the fill flag flips
    // mid-transition and the visible scale bounces).
    let settled_upper = Rect::from_min_max(
        content.min,
        Pos2::new(
            content.max.x,
            band_top_at(if emu.list_open { 1.0 } else { 0.0 }),
        ),
    );

    let painter = ui.painter().clone();
    // Row 0's Border Background=#FF000000 — a pure black item area.
    painter.rect_filled(upper, 0.0, Color32::BLACK.gamma_multiply(alpha));

    // Diagnostics: AES_AUTOLAUNCH=<rom path> opens the game's section
    // and launches it through the real UI path — the same call chain
    // a carousel double-click takes (open_section → launch →
    // Session::start → viewport capture). Lets a harness reproduce
    // app-side launches without UI automation.
    static AUTOLAUNCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !AUTOLAUNCH.swap(true, std::sync::atomic::Ordering::Relaxed) {
        if let Ok(rom) = std::env::var("AES_AUTOLAUNCH") {
            let rom = PathBuf::from(rom);
            match emu.sections.iter().position(|sec| {
                cfg.games
                    .get(&section_key(sec))
                    .is_some_and(|gs| gs.iter().any(|g| PathBuf::from(g) == rom))
            }) {
                Some(i) => {
                    emu.open_section(i, cfg);
                    emu.launch(cfg, &rom);
                }
                None => {
                    emu.notice = Some(format!("AES_AUTOLAUNCH: {rom:?} is not in any section"));
                }
            }
        }
    }

    // The overlays own the pointer while one is up — cards, the
    // section strip and the band stop receiving input.
    let interactive = interactive
        && !emu.editor.is_open()
        && !emu.patches.is_open()
        && !emu.cfg_ov.is_open()
        && !emu.rpcs3_patches.is_open()
        && !emu.shad_patches.is_open()
        && !emu.shad_cheats.is_open()
        && !emu.cemu_packs.is_open()
        && !emu.prof_prompt.is_open()
        && !emu.upd_open;

    // open_section flagged a missing Xenia profile — open the prompt
    // (deferred to here because open() seeds the fade on the ctx).
    if emu.prof_want {
        emu.prof_want = false;
        emu.prof_prompt.open(ui.ctx());
    }

    // The section-entry update check — resolves on a worker; a real
    // version bump opens the same confirm dialog the fullscreen-exit
    // prompt uses. A failed/ambiguous lookup quietly drops the check.
    if let Some((key, check)) = &emu.upd_check {
        let done = check.lock().unwrap_or_else(|e| e.into_inner()).done.clone();
        if let Some((installed, latest)) = done {
            let key = key.clone();
            emu.upd_check = None;
            if let Some(spec) = handler_spec(&key) {
                if let Some(latest) = latest {
                    if installed.as_deref() != Some(latest.as_str()) {
                        emu.upd_key = key;
                        emu.upd_subtitle = format!(
                            "{}: {} installed → {} available",
                            spec.name,
                            installed.unwrap_or_else(|| "unknown".into()),
                            latest
                        );
                        emu.upd_sel = 0;
                        emu.upd_open = true;
                    }
                }
            }
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    // Async launch — the worker merged the custom config and spawned
    // Xenia; the session lands here.
    if let Some(rx) = &emu.launch_rx {
        match rx.try_recv() {
            Ok(Ok(s)) => {
                // ReloadArcadeLockedPillarboxCropFromMetadata — the
                // per-ROM locked crop wins over auto-detection.
                emu.pillar_lock = crate::emulation::emu_meta::locked_crop(&s.game);
                emu.aspect_applied = false;
                // Fresh capture — the old ring/seqs belong to the
                // dead session.
                emu.gpu_cap = None;
                emu.gpu_failed_gen = 0;
                emu.gpu_err = None;
                emu.slot_seq = 0;
                emu.frame_seq = 0;
                emu.session = Some(s);
                emu.launch_rx = None;
                emu.notice = None;
                emu.stats_instant = None;
                emu.frametimes.clear();
                // A fresh launch always opens on the game — the
                // presentation fades in over the carousel.
                emu.viewport_dismissed = false;
            }
            Ok(Err(e)) => {
                xenia::session_log(&format!("launch failed: {e}"));
                emu.notice = Some(e);
                emu.launch_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => ui.ctx().request_repaint(),
            Err(mpsc::TryRecvError::Disconnected) => {
                xenia::session_log("launch worker died before producing a session");
                emu.notice = Some("Launch failed (worker died — see session.log)".into());
                emu.launch_rx = None;
            }
        }
    }

    // Folder scan finished on the worker — merge what it found.
    if let Some(rx) = &emu.scan_rx {
        match rx.try_recv() {
            Ok(paths) => {
                emu.scan_rx = None;
                emu.notice = (paths.is_empty()).then(|| "No ROMs found in that folder".to_string());
                emu.add_games(cfg, paths);
            }
            Err(mpsc::TryRecvError::Empty) => ui.ctx().request_repaint(),
            Err(mpsc::TryRecvError::Disconnected) => {
                emu.scan_rx = None;
                emu.notice = Some("Folder scan failed.".into());
            }
        }
    }

    // "Reboot" from the capture context menu — close the session
    // (Drop kills the process) and relaunch the same game.
    if emu.reboot {
        emu.reboot = false;
        if let Some(game) = emu.session.take().map(|s| s.game.clone()) {
            emu.launch(cfg, &game);
        }
    }

    // ApplyCaptureAspectRatioToRunningTarget — once the worker
    // resolves the render window, resize it to the section's saved
    // aspect ("handler" leaves the emulator's own).
    #[cfg(windows)]
    if !emu.aspect_applied {
        if let Some(session) = &emu.session {
            let hwnd = session.hwnd();
            if !hwnd.is_invalid() {
                if let Some(ratio) = aspect_ratio_of(&emu.render.aspect_key) {
                    xenia::apply_aspect_ratio(hwnd, ratio);
                }
                emu.aspect_applied = true;
            }
        }
    }
    #[cfg(not(windows))]
    {
        emu.aspect_applied = true;
    }

    // Session teardown once the emulator reports exit. The last
    // captured frame stays mounted so the presentation layer can fade
    // out over the carousel instead of snapping away (Lacrima's
    // CapturePresentationOpacity DoubleTransition).
    if emu.session.as_mut().is_some_and(|s| s.finished()) {
        emu.session = None;
        emu.viewport_dismissed = false;
        emu.game_fs = false;
        emu.input_tun = false;
        // TryLaunchPendingEmulatorRequest — the queued double-click
        // fires only once the replaced session is fully reaped.
        if let Some(game) = emu.pending_launch.take() {
            emu.launch(cfg, &game);
        }
    }

    // The opaque capture layer fades in/out over the carousel —
    // ~0.3s, same duration as Lacrima's transition.
    let cap_target = emu.session.is_some() && !emu.viewport_dismissed;
    let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
    let before = emu.cap_t;
    emu.cap_t = (emu.cap_t + if cap_target { dt / 0.3 } else { -dt / 0.3 }).clamp(0.0, 1.0);
    let cap_a = crate::ease_in_out_cubic(emu.cap_t);
    if emu.cap_t > 0.0 || emu.cap_t != before {
        ui.ctx().request_repaint();
    } else if emu.session.is_some() {
        // Hidden session — the layer isn't painting frames, so only
        // poll occasionally for emulator exit instead of repainting
        // every vsync.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
    // The fade finished — drop the stale frame texture + ring.
    if emu.session.is_none() && emu.cap_t <= 0.0 {
        emu.frame_tex = None;
        emu.gpu_cap = None;
        emu.frame_seq = 0;
        emu.slot_seq = 0;
        emu.frame_buf = Vec::new();
    }

    // Live render options → the capture thread's swizzle reads these
    // atomics each frame, so panel edits land on the very next
    // captured frame.
    if let Some(session) = &emu.session {
        use std::sync::atomic::Ordering::Relaxed;
        let r = session.render();
        r.brightness.store(emu.render.brightness.to_bits(), Relaxed);
        r.saturation.store(emu.render.saturation.to_bits(), Relaxed);
        let [tr, tg, tb] = emu.render.tint;
        r.tint
            .store((tr as u32) << 16 | (tg as u32) << 8 | tb as u32, Relaxed);
    }
    // Per-section write-through (Lacrima's SectionSettings AutoSave).
    if emu.render_dirty {
        emu.render_dirty = false;
        if let Some(k) = emu.loaded_key.clone() {
            cfg.render.insert(k, emu.render.clone());
        }
        emu.settings_dirty = true;
    }
    // DisableVSync — Lacrima unthrottles the capture host's render
    // loop. Our presentation is repaint-driven, so the equivalent is
    // a continuous repaint while a session is visible: frames
    // present at display rate regardless of capture arrival timing.
    if emu.render.disable_vsync && emu.session.is_some() && !emu.viewport_dismissed {
        ui.ctx().request_repaint();
    }

    // The "first ROM cover" toggle re-themes every section tile.
    if cfg.use_first_item_cover != emu.prev_first_cover {
        emu.prev_first_cover = cfg.use_first_item_cover;
        emu.refresh_section_covers(cfg);
    }

    // Resolved titles + fetched covers land on the card items.
    emu.drain_meta(cfg, ui.ctx());

    // The carousel stays mounted under the capture layer — cards keep
    // receiving input only once the layer is mostly faded out. While
    // the layer is fully opaque the cards aren't painted at all.
    if emu.active.is_some() && cap_a < 0.999 {
        let layout = cfg.cover_layout_mode.min(2);
        draw_items(
            ui,
            upper,
            emu,
            cfg,
            car_cfg,
            layout,
            alpha,
            interactive && cap_a < 0.5,
            s,
        );
    } else {
        painter.text(
            upper.center(),
            Align2::CENTER_CENTER,
            "No album loaded",
            FontId::proportional(30.0 * s),
            Color32::from_gray(110).gamma_multiply(alpha),
        );
    }
    // Game-fullscreen rides on the app's real fullscreen window mode
    // (the F12 path): entering sends Fullscreen(true), leaving restores
    // the previous windowed/maximized state — but only when we were
    // the ones who drove it. If the user leaves fullscreen through
    // the app (F12 / the exit prompt), game_fs follows instead of
    // fighting it.
    let (win_fs, win_max) = ui.ctx().input(|i| {
        (
            i.viewport().fullscreen.unwrap_or(false),
            i.viewport().maximized.unwrap_or(false),
        )
    });
    if emu.game_fs && !win_fs && !emu.fs_drove {
        emu.fs_drove = true;
        emu.fs_seen = false;
        emu.fs_prev_max = win_max;
        // set_fullscreen pairs Fullscreen with Decorations(false) —
        // the bare command leaves winit's 1px shadow inset, a
        // desktop strip across the top of the fullscreen capture.
        crate::set_fullscreen(ui.ctx(), true);
    }
    if emu.fs_drove && win_fs {
        emu.fs_seen = true; // the request landed at least once
    }
    if emu.fs_drove && emu.fs_seen && !win_fs {
        // The window left fullscreen without us (F12, exit prompt) —
        // the flag follows the actual state.
        emu.fs_drove = false;
        emu.fs_seen = false;
        emu.game_fs = false;
    }
    if !emu.game_fs && emu.fs_drove && win_fs {
        // Restore exactly what was there before: leave fullscreen and
        // put back the pre-entry maximized flag.
        emu.fs_drove = false;
        emu.fs_seen = false;
        crate::set_fullscreen(ui.ctx(), false);
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(emu.fs_prev_max));
    }
    // Lacrima's IsFullscreen: the capture stretches over the whole
    // view (which, in fullscreen, is the whole screen — the title bar
    // collapses too) and the band/strip/search disappear.
    let fs = emu.game_fs && win_fs && emu.session.is_some() && !emu.viewport_dismissed;
    let pres_target = if fs { content } else { upper };
    // Presentation motion: the rect follows its target 1:1 — the
    // album strip's own eased slide drives the capture directly.
    // The ONLY discrete retarget is `fs` flipping (fullscreen enter/
    // exit jumps between `upper` and `content`); that engages a
    // fixed 0.45s cubic tween from wherever the rect currently is.
    // No delta heuristics — a mid-slide frame hitch moves the target
    // and the capture together instead of stalling the capture.
    {
        if !emu.pres_anim.is_finite() {
            emu.pres_anim = pres_target;
            emu.pres_fs = fs;
            emu.pres_t = 1.0;
        }
        if fs != emu.pres_fs {
            emu.pres_fs = fs;
            emu.pres_from = emu.pres_anim;
            emu.pres_t = 0.0;
        }
        if emu.pres_t < 1.0 {
            emu.pres_t = (emu.pres_t + dt / 0.45).min(1.0);
            let e = crate::ease_in_out_cubic(emu.pres_t);
            emu.pres_anim = if emu.pres_t < 1.0 {
                lerp_rect(emu.pres_from, pres_target, e)
            } else {
                pres_target
            };
            ui.ctx().request_repaint();
        } else {
            emu.pres_anim = pres_target;
        }
    }
    let pres_rect = emu.pres_anim;
    // Motion debug — while the strip slide or the fs tween runs,
    // record the presentation rect trajectory + frame dt to
    // motion.log so capture stalls are diagnosable from real data.
    {
        let endpoint = if emu.list_open { 1.0 } else { 0.0 };
        let moving = (list_t - endpoint).abs() > 0.001 || emu.pres_t < 1.0;
        if moving && emu.motion_log.is_none() {
            if let Some(f) = settings::data_root()
                .map(|d| d.join("motion.log"))
                .and_then(|p| std::fs::File::create(p).ok())
            {
                emu.motion_log = Some(f);
                emu.motion_t0 = std::time::Instant::now();
            }
        }
        if !moving {
            emu.motion_log = None;
        }
    }
    // Hide the OS cursor while the game owns the screen — except
    // while the input tunnel owns it, when the cursor is the user's
    // pointer into the emulator's own UI.
    let want_hidden = fs && !emu.input_tun;
    if want_hidden != emu.cursor_hidden {
        emu.cursor_hidden = want_hidden;
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::CursorVisible(!want_hidden));
    }
    if emu.cap_t > 0.0 {
        // `stable` is the destination the slide is heading TO — the
        // fullscreen content rect, or `upper` at list_t's endpoint.
        // Fit/fill decisions key off this so nothing mid-transition
        // can flip the crop state.
        let stable = if fs { content } else { settled_upper };
        draw_session(
            ui,
            &painter,
            pres_rect,
            stable,
            emu,
            alpha * cap_a,
            s,
            cfg.back_cover_letterbox_fill,
        );
    }
    // The input tunnel — while it owns the session, pointer/key
    // events inside the frame rect PostMessage to the hidden
    // emulator window so its own UI is reachable. Esc releases it
    // (handled in the global Esc chain, which runs after this — the
    // forward loop just never sends Escape).
    tunnel_input(ui, emu, interactive && cap_a > 0.5);
    // Gestures on the capture layer (Lacrima's CaptureContextMenuLayer
    // + double-click): double-click toggles game-fullscreen, right-
    // click opens the session menu. The interact region also stops
    // clicks from reaching the cards underneath.
    if emu.session.is_some() && cap_a > 0.5 {
        let cap = ui.interact(pres_rect, ui.id().with("emu-capture-layer"), Sense::click());
        if cap.double_clicked() && !emu.input_tun {
            emu.toggle_fullscreen();
        }
        let mut open_editor = false;
        let mut stop = false;
        cap.context_menu(|ui| {
            // Lacrima's CaptureContextMenuLayer menu.
            if ui.button("Edit Metadata").clicked() {
                open_editor = true;
                ui.close();
            }
            // SetCaptureAspectRatio — resizes the emulator window so
            // the capture's native aspect changes; saved per section.
            ui.menu_button("Aspect Ratio", |ui| {
                for (key, label) in [
                    ("handler", "Handler Default"),
                    ("4:3", "4:3"),
                    ("16:9", "16:9"),
                    ("21:9", "21:9"),
                    ("32:9", "32:9"),
                    ("1:1", "1:1"),
                ] {
                    if ui
                        .selectable_label(emu.render.aspect_key == key, label)
                        .clicked()
                    {
                        if emu.render.aspect_key != key {
                            emu.render.aspect_key = key.to_string();
                            emu.render_dirty = true;
                            #[cfg(windows)]
                            if let Some(session) = &emu.session {
                                let hwnd = session.hwnd();
                                if !hwnd.is_invalid() {
                                    if let Some(ratio) = aspect_ratio_of(key) {
                                        xenia::apply_aspect_ratio(hwnd, ratio);
                                    }
                                    emu.aspect_applied = true;
                                }
                            }
                        }
                        ui.close();
                    }
                }
            });
            // Use Current Crop Size — persist the live resolved crop
            // into the ROM's .meta (SaveCurrentArcadePillarboxCrop).
            if ui.button("Use Current Crop Size").clicked() {
                // Resolved insets (TryGetPillarboxCrop's _cropLeft/
                // _cropRight): the debounced target once detected —
                // which is also what a live lock resolves to — with
                // the animating value as the mid-tween fallback.
                let (l, r) = if emu.pillar_tgt != (0, 0) {
                    emu.pillar_tgt
                } else {
                    (
                        emu.pillar_cur.0.round() as u32,
                        emu.pillar_cur.1.round() as u32,
                    )
                };
                // Frame width: the GPU ring on the zero-copy path —
                // frame_tex only exists on the CPU mirror.
                let fw = emu
                    .gpu_cap
                    .as_ref()
                    .map(|g| g.size[0] as u32)
                    .or_else(|| emu.frame_tex.as_ref().map(|t| t.size()[0] as u32))
                    .unwrap_or(0);
                if let Some(session) = &emu.session {
                    if (l > 0 || r > 0) && fw > 0 {
                        crate::emulation::emu_meta::save_locked_crop(&session.game, l, r, fw);
                        emu.pillar_lock = Some((l, r, fw));
                    }
                }
                if !emu.render.remove_pillars {
                    emu.render.remove_pillars = true;
                    emu.render_dirty = true;
                }
                ui.close();
            }
            // Clear Saved Crop Size — drop the locked crop; detection
            // resumes next frame (ClearArcadeLockedPillarboxCrop).
            if emu.pillar_lock.is_some() {
                if ui.button("Clear Saved Crop Size").clicked() {
                    if let Some(session) = &emu.session {
                        crate::emulation::emu_meta::clear_locked_crop(&session.game);
                    }
                    emu.pillar_lock = None;
                    ui.close();
                }
            }
            if ui.button("Reboot").clicked() {
                emu.reboot = true;
                ui.close();
            }
            ui.separator();
            if ui
                .button(if emu.game_fs {
                    "Exit Fullscreen"
                } else {
                    "Fullscreen"
                })
                .clicked()
            {
                emu.toggle_fullscreen();
                ui.close();
            }
            if ui.button("Close Emulator").clicked() {
                stop = true;
                ui.close();
            }
        });
        if open_editor {
            let g = emu.session.as_ref().unwrap().game.clone();
            let x360 = emu.loaded_key.as_deref() == Some(XENIA_SECTION);
            emu.editor.open(ui.ctx(), g, String::new(), x360);
        }
        if stop {
            emu.request_close();
        }
    }
    // In game-fullscreen the chrome (band, strip, search, overlays'
    // underlying view) is not painted at all.
    let chrome_hidden = fs && cap_a > 0.5;

    if !chrome_hidden {
        // Row 1 — the 140px control band (Background #E6000000 plus the
        // top Separator that every Lacrima chrome band draws).
        painter.rect_filled(band, 0.0, Color32::from_black_alpha((230.0 * alpha) as u8));
        let line = Stroke::new(s.max(1.0), Color32::from_white_alpha((51.0 * alpha) as u8));
        painter.hline(content.min.x..=content.max.x, band.min.y, line);
        if list_t > 0.001 {
            painter.hline(content.min.x..=content.max.x, menu_top, line);
            painter.rect_filled(
                menu_band,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: radius,
                    se: radius,
                },
                Color32::BLACK.gamma_multiply(alpha * list_t),
            );
        }

        // Left: the section-list toggle (StackPanel Margin 50,0,0,0,
        // 60px button — the bar-over-square glyph, lit while open).
        let cy = band.center().y;
        let btn = Rect::from_center_size(
            Pos2::new(band.min.x + 50.0 * s + 30.0 * s, cy),
            Vec2::splat(60.0 * s),
        );
        let r = if interactive {
            ui.interact(btn, ui.id().with("emu-list-toggle"), Sense::click())
        } else {
            ui.interact(btn, ui.id().with("emu-list-toggle"), Sense::hover())
        }
        .on_hover_text(if emu.list_open {
            "Hide Albums"
        } else {
            "Show Albums"
        });
        if r.clicked() {
            emu.list_open = !emu.list_open;
        }
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        transport::icon_album_list(&painter, btn, emu.list_open, alpha);

        // Right cluster packs from the edge — session-only buttons take
        // the outer slots: [layout][setup][sliders][tun][power], shrinking
        // to [layout][setup] while no capture runs (the capture-settings
        // and input-tunnel toggles only exist while a game is captured).
        // Narrow bands: a slot that would reach into the centered pause
        // button parks in the left gap instead — there's room there.
        let pause = Rect::from_center_size(band.center(), Vec2::splat(80.0 * s));
        let gap = 10.0 * s;
        let session_on = emu.session.is_some();
        // The viewport toggle lives at min+190 — clamp it left of the
        // pause zone so a narrow band can't overlap the button either.
        let vp_cx = (band.min.x + 190.0 * s)
            .min(pause.min.x - gap - 30.0 * s)
            .max(band.min.x + 140.0 * s);
        // Occupied lane centers — the left toggles first; parked and
        // packed slots append so a late slot can't land on them.
        let mut taken = vec![band.min.x + 80.0 * s];
        if session_on {
            taken.push(vp_cx);
        }
        let mut left_next = *taken.last().unwrap() + 110.0 * s;
        // The skipped-update icon takes a slot when the active
        // section's handler has a pending update the user parked.
        let upd_key = emu
            .active
            .and_then(|i| emu.sections.get(i))
            .map(|s| section_key(s));
        let upd_sub = upd_key.as_ref().and_then(|k| emu.upd_skipped.get(k).cloned());
        // Balance split: when the free lane left and right of the
        // pause fits the same icon count, the innermost right-cluster
        // slots park left so both sides read equal (3 | pause | 3).
        let upd_on = upd_sub.is_some() && !emu.upd_open;
        let setup_on = !session_on && emu.launch_rx.is_none();
        let n_right = [session_on, session_on, session_on, upd_on, setup_on, true]
            .iter()
            .filter(|&&c| c)
            .count();
        let rx0 = band.max.x - 50.0 * s - 30.0 * s;
        let mut rx_probe = rx0;
        let mut right_cap = 0usize;
        while rx_probe - 30.0 * s >= pause.max.x + gap {
            right_cap += 1;
            rx_probe -= 110.0 * s;
        }
        let mut left_cap = 0usize;
        let mut lx = left_next;
        while lx + 30.0 * s <= pause.min.x - gap {
            left_cap += 1;
            lx += 110.0 * s;
        }
        let spill_from = band_spill_from(taken.len(), n_right, right_cap, left_cap);
        let mut rx = rx0;
        let mut present = 0usize;
        let mut slot = |cond: bool| -> f32 {
            let force = if cond {
                let i = present;
                present += 1;
                i >= spill_from
            } else {
                false
            };
            band_slot(cond, force, &mut rx, &mut left_next, &mut taken, pause, s)
        };
        let pw_x = slot(session_on);
        let tun_x = slot(session_on);
        let sliders_x = slot(session_on);
        let upd_x = slot(upd_on);
        // The setup tile hides while a game captures or launches —
        // the dimmed app icon reads as noise over the band.
        let setup_x = slot(setup_on);
        let layout_x = slot(true);

        // Capture-settings toggle (Lacrima's ToggleRenderOptions) — the
        // sliders glyph; only exists while a session captures.
        if !sliders_x.is_nan() {
            let rb = Rect::from_center_size(Pos2::new(sliders_x, cy), Vec2::splat(60.0 * s));
            let r = if interactive {
                ui.interact(rb, ui.id().with("emu-render"), Sense::click())
            } else {
                ui.interact(rb, ui.id().with("emu-render"), Sense::hover())
            }
            .on_hover_text("Capture Settings");
            if r.clicked() {
                emu.render_open = !emu.render_open;
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            transport::icon_sliders(&painter, rb, emu.render_open, alpha);
        }

        // Input-tunnel toggle — while lit, clicks/keys inside the frame
        // reach the emulator's hidden window so its own dialogs are
        // usable in the captured stream. On Linux the same applies when
        // the session runs embedded in the nested compositor — an
        // external-window fallback owns its input itself.
        let tun_ok = if cfg!(windows) {
            true
        } else {
            emu.session.as_ref().is_some_and(|s| s.embedded())
        };
        if tun_ok && !tun_x.is_nan() {
            let tb = Rect::from_center_size(Pos2::new(tun_x, cy), Vec2::splat(60.0 * s));
            let r = if interactive {
                ui.interact(tb, ui.id().with("emu-tun"), Sense::click())
            } else {
                ui.interact(tb, ui.id().with("emu-tun"), Sense::hover())
            }
            .on_hover_text(if emu.input_tun {
                "Stop Controlling Emulator (Esc)"
            } else {
                "Control Emulator — clicks and keys reach the game"
            });
            if r.clicked() {
                emu.toggle_input_tunnel();
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            transport::icon_pointer(&painter, tb, emu.input_tun, alpha);
        }

        // Skipped-update icon — reopens the parked update prompt for
        // the active section (its stored subtitle still describes the
        // found version bump).
        if !upd_x.is_nan() {
            let ub = Rect::from_center_size(Pos2::new(upd_x, cy), Vec2::splat(60.0 * s));
            let r = if interactive {
                ui.interact(ub, ui.id().with("emu-update"), Sense::click())
            } else {
                ui.interact(ub, ui.id().with("emu-update"), Sense::hover())
            }
            .on_hover_text(upd_sub.clone().unwrap_or_else(|| "Update available".into()));
            if r.clicked() {
                if let (Some(k), Some(sub)) = (upd_key.clone(), upd_sub.clone()) {
                    emu.upd_key = k;
                    emu.upd_subtitle = sub;
                    emu.upd_sel = 0;
                    emu.upd_open = true;
                }
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            transport::icon_update(&painter, ub, alpha);
        }

        // Handler-setup launch (LaunchCurrentSectionHandlerSetup) —
        // spawns the configured emulator with no game so the user can
        // reach its own settings. Disabled while a session runs or a
        // launch is in flight (Lacrima's CanLaunch… guard).
        let setup_exe = emu.setup_exe(cfg);
        let setup_ok = setup_exe.is_some() && !setup_x.is_nan();
        // The handler's own exe icon (ExtractIconEx → 48px) inside a
        // rounded-rect button like Lacrima's setup tile — hidden
        // entirely while a game captures/launches (`setup_x` is NaN),
        // dimmed otherwise when unavailable. The texture stays warm
        // so the tile pops back fully rendered.
        if let Some(p) = &setup_exe {
            let key = p.to_string_lossy().into_owned();
            if emu.handler_icon_key.as_deref() != Some(key.as_str()) {
                emu.handler_icon_key = Some(key);
                emu.handler_icon = crate::exe_icon::load(p).map(|img| {
                    ui.ctx()
                        .load_texture("handler-icon", img, egui::TextureOptions::LINEAR)
                });
            }
        }
        if !setup_x.is_nan() {
            let sb = Rect::from_center_size(Pos2::new(setup_x, cy), Vec2::splat(60.0 * s));
            let r = if interactive && setup_ok {
                ui.interact(sb, ui.id().with("emu-setup"), Sense::click())
            } else {
                ui.interact(sb, ui.id().with("emu-setup"), Sense::hover())
            }
            .on_hover_text(if setup_ok {
                "Open Emulator Settings".to_string()
            } else {
                "Open Emulator Settings — no emulator configured".to_string()
            });
            if r.clicked() {
                if let Some(exe) = &setup_exe {
                    let kind = handler_kind(emu.active_handler(cfg).as_deref());
                    if let Err(e) = xenia::launch_setup(&exe.to_string_lossy(), kind) {
                        emu.notice = Some(e);
                    }
                }
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            let icon_a = alpha * if setup_ok { 1.0 } else { 0.45 };
            let bgr = sb.shrink(4.0 * s);
            painter.rect_filled(
                bgr,
                CornerRadius::same((9.0 * s) as u8),
                Color32::from_rgb(0x2D, 0x2D, 0x2D).gamma_multiply(icon_a),
            );
            painter.rect_stroke(
                bgr,
                CornerRadius::same((9.0 * s) as u8),
                Stroke::new(s.max(1.0), Color32::from_white_alpha((40.0 * icon_a) as u8)),
                StrokeKind::Inside,
            );
            // The icon fills most of the button — Lacrima's setup tile is
            // icon-dominant, not a chip in a big button.
            let ir = bgr.shrink(6.0 * s);
            if let Some(tex) = &emu.handler_icon {
                painter.image(
                    tex.id(),
                    ir,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE.gamma_multiply(icon_a),
                );
            } else {
                // Fallback — a gear: hub circle + eight spokes, line-art style.
                let c = Color32::from_gray(170).gamma_multiply(icon_a);
                let stroke = Stroke::new(2.4 * s, c);
                let ctr = ir.center();
                let r_out = ir.height() * 0.42;
                let r_in = ir.height() * 0.28;
                painter.circle_stroke(ctr, ir.height() * 0.16, stroke);
                for i in 0..8 {
                    let a = i as f32 * std::f32::consts::FRAC_PI_4;
                    painter.add(Shape::line_segment(
                        [
                            ctr + Vec2::new(a.cos() * r_in, a.sin() * r_in),
                            ctr + Vec2::new(a.cos() * r_out, a.sin() * r_out),
                        ],
                        stroke,
                    ));
                }
            }
        }

        // Session controls — Lacrima's IsEmulatorRunning button set:
        // left adds the viewport toggle (switch list ⇄ game), the center
        // holds pause/play (process suspend/resume), and the right adds
        // the Close-Emulator power button at the far end.
        let mut toggle_viewport = false;
        let mut toggle_pause = false;
        let mut close_emu = false;
        if let Some(session) = &emu.session {
            // Viewport toggle — directly right of the albums toggle
            // (same 110px button spacing the right cluster uses),
            // clamped off the pause zone above.
            let vp = Rect::from_center_size(Pos2::new(vp_cx, cy), Vec2::splat(60.0 * s));
            let r = if interactive {
                ui.interact(vp, ui.id().with("emu-viewport"), Sense::click())
            } else {
                ui.interact(vp, ui.id().with("emu-viewport"), Sense::hover())
            }
            .on_hover_text(if emu.viewport_dismissed {
                "Back to Game"
            } else {
                "Switch to List"
            });
            toggle_viewport = r.clicked();
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            transport::icon_viewport(&painter, vp, !emu.viewport_dismissed, alpha);

            // Center pause/play — NtSuspendProcess/NtResumeProcess, the
            // glyph is Lacrima's circled bars/triangle.
            let pp = pause;
            let r = if interactive {
                ui.interact(pp, ui.id().with("emu-pause"), Sense::click())
            } else {
                ui.interact(pp, ui.id().with("emu-pause"), Sense::hover())
            }
            .on_hover_text(if session.is_paused() {
                "Resume Emulator"
            } else {
                "Pause Emulator"
            });
            toggle_pause = r.clicked();
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            transport::icon_play(&painter, pp, !session.is_paused(), alpha);

            // Close Emulator — the power glyph at the right end of the
            // band (CloseEmulatorCommand), next to the layout icon which
            // slides one slot left to make room.
            let pw = Rect::from_center_size(Pos2::new(pw_x, cy), Vec2::splat(60.0 * s));
            let r = if interactive {
                ui.interact(pw, ui.id().with("emu-close"), Sense::click())
            } else {
                ui.interact(pw, ui.id().with("emu-close"), Sense::hover())
            }
            .on_hover_text("Close Emulator");
            close_emu = r.clicked();
            if r.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            transport::icon_power(&painter, pw, alpha);
        }
        if toggle_viewport {
            emu.toggle_viewport();
        }
        if toggle_pause {
            if let Some(session) = &emu.session {
                session.set_paused(!session.is_paused());
            }
        }
        if close_emu {
            emu.request_close();
        }

        // Right: the cover-layout cycle (Margin 0,0,50,0) — persists
        // Emulation.CoverLayoutMode, independent of the music band's
        // Carousel.LayoutMode (Lacrima binds one global CoverLayoutMode
        // to both). While a session runs it makes room for the power
        // button (Lacrima's right StackPanel order: layout … power …
        // volume).
        let right_x = layout_x;
        let btn = Rect::from_center_size(Pos2::new(right_x, cy), Vec2::splat(60.0 * s));
        let r = if interactive {
            ui.interact(btn, ui.id().with("emu-layout"), Sense::click())
        } else {
            ui.interact(btn, ui.id().with("emu-layout"), Sense::hover())
        }
        .on_hover_text("Cover Layout");
        if r.clicked() {
            cfg.cover_layout_mode = (cfg.cover_layout_mode + 1) % 3;
            emu.settings_dirty = true;
        }
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        transport::icon_layout(&painter, btn, cfg.cover_layout_mode.min(2), alpha);

        // Row 2 — the section strip slides down out of view while the
        // band holds (same trick as the music album row).
        if list_t > 0.001 {
            let slide = row_rect.translate(Vec2::new(
                0.0,
                (1.0 - list_t) * (content.max.y - row_rect.min.y),
            ));
            match emu.row.ui(
                ui,
                slide,
                &mut emu.sections,
                alpha * list_t,
                interactive && list_t > 0.999,
                s,
                false,
                false,
            ) {
                // Like the music album row: a single click only selects
                // the tile — opening a section requires a double-click.
                // The payload is the tile's `emu:` dir — resolved back
                // to a position here (the row may render a filtered
                // subset, so indices don't survive the round trip).
                album_row::Action::Opened(dir) => {
                    if let Some(i) = emu
                        .sections
                        .iter()
                        .position(|s| s.dir.to_string_lossy() == dir)
                    {
                        // Load the section's persisted games into the card
                        // view on first activation — the key travels on the
                        // tile's `emu:` dir so reordering can't miskey it.
                        emu.activate_section(i, cfg);
                    }
                }
                _ => {}
            }
            // A landed section reorder persists (Lacrima's AlbumOrder) —
            // the strip restores this order next launch. The loaded-
            // section marker follows its key across the permutation so
            // `active` can't end up pointing at the wrong console.
            if emu.row.take_order_dirty() {
                cfg.section_order = emu.sections.iter().map(section_key).collect();
                if let Some(key) = emu.loaded_key.clone() {
                    emu.active = emu.sections.iter().position(|s| section_key(s) == key);
                }
                emu.settings_dirty = true;
            }
        }

        // Item search pill docked top-right (RomSearch — same slot as the
        // music view's item search). Lacrima hides the search box while
        // the capture layer is up — it fades with the rest of the chrome.
        if cap_a < 0.999 {
            emu.search.ui(
                ui,
                content.max.x - 30.0 * s,
                content.min.y + 26.0 * s,
                "Search",
                true,
                false,
                alpha * (1.0 - cap_a),
                interactive && cap_a < 0.05,
                s,
            );
        }

        // The capture render-options panel (Lacrima's
        // RenderOptionsOverlay) — slides in from the right over the
        // item area only; the icon band and section strip stay
        // visible like the global settings overlay keeps its chrome.
        let item_area = Rect::from_min_max(content.min, Pos2::new(content.max.x, band.min.y));
        render_panel(ui, emu, item_area, alpha, interactive, s);
    } // !chrome_hidden

    // The ROM metadata editor paints last — above the cards, the
    // strip and the search pill.
    if emu.editor.is_open() {
        emu.editor.ui(ui, content, s);
        // A save rewrites the sidecars — refresh the card's title +
        // cover (and the section tile when it wears the first cover).
        if let Some(saved) = emu.editor.take_saved() {
            if let Some(t) = emu_meta::cached_title(&saved) {
                emu.cards.apply_info(
                    &saved,
                    &library::TrackInfo {
                        title: t,
                        artist: String::new(),
                        album: String::new(),
                        number: None,
                        cover_url: None,
                        duration: None,
                    },
                );
            }
            let cov = emu_meta::cover_path(&saved);
            if cov.exists() {
                let src = library::CoverSrc::File(cov);
                emu.row.invalidate_cover(&src);
                emu.cards.set_cover(&saved, src);
            } else {
                // Cover removed — back to the console art fallback.
                emu.row
                    .invalidate_cover(&library::CoverSrc::File(cov.clone()));
                if let Some(fb) = emu.fallback_cover() {
                    emu.cards.set_cover(&saved, fb);
                }
            }
            emu.refresh_section_covers(cfg);
        }
    }
    // The Xenia overlays paint above everything else in the view.
    if emu.patches.is_open() {
        emu.patches.ui(ui, content, s);
    }
    if emu.cfg_ov.is_open() {
        emu.cfg_ov.ui(ui, content, s);
    }
    if emu.rpcs3_patches.is_open() {
        emu.rpcs3_patches.ui(ui, content, s);
    }
    if emu.shad_patches.is_open() {
        emu.shad_patches.ui(ui, content, s);
    }
    if emu.shad_cheats.is_open() {
        emu.shad_cheats.ui(ui, content, s);
    }
    if emu.cemu_packs.is_open() {
        emu.cemu_packs.ui(ui, content, s);
    }
    if emu.prof_prompt.is_open() {
        if let Some(Some(name)) = emu.prof_prompt.ui(ui, content, s) {
            emu.notice = Some(match xenia_storage(cfg) {
                Some(root) => match xenia_profile::create(&root, &name) {
                    Ok(_) => {
                        format!("Xenia profile \"{name}\" created — it will sign in on launch.")
                    }
                    Err(e) => format!("Failed to create the Xenia profile: {e}"),
                },
                None => "Failed to create the Xenia profile — no storage root.".into(),
            });
        }
    }

    // Update-available prompt — the same confirm dialog the
    // fullscreen-exit/delete prompts use. Update swaps it into a
    // progress/status view that stays up until the worker finishes;
    // Skip parks an update icon in the band for the section.
    if emu.upd_open || emu.upd_vis > 0.001 {
        let dt = ui.ctx().input(|i| i.stable_dt);
        emu.upd_vis = (emu.upd_vis + dt / if emu.upd_open { 0.18 } else { -0.14 })
            .clamp(0.0, 1.0);
        let (upd_key, upd_subtitle) = (emu.upd_key.clone(), emu.upd_subtitle.clone());
        if emu.upd_job.is_none() {
            match crate::confirm_overlay(
                ui,
                content,
                "Update available",
                &upd_subtitle,
                "Update",
                "Skip",
                &mut emu.upd_sel,
                s,
                emu.upd_vis,
                emu.upd_open,
                "emu-upd",
            ) {
                crate::DialogAction::Confirm => {
                    emu.upd_skipped.remove(&upd_key);
                    if let Some(spec) = handler_spec(&upd_key) {
                        // A shared handler (RetroArch, Dolphin) may
                        // already be downloading under a sibling
                        // section's key — never run two installs of
                        // the same emulator into the same directory.
                        if !emu.dl.values().any(|j| j.handler == spec.handler) {
                            let shared = (spec.download)();
                            emu.upd_job = Some(shared.clone());
                            emu.dl.insert(
                                upd_key.clone(),
                                DlJob {
                                    handler: spec.handler,
                                    name: spec.name,
                                    cores: None,
                                    shared,
                                },
                            );
                            emu.dl_msg.remove(&upd_key);
                        } else {
                            emu.upd_open = false;
                        }
                    }
                }
                crate::DialogAction::Cancel => {
                    emu.upd_open = false;
                    emu.upd_skipped.insert(upd_key, upd_subtitle);
                }
                crate::DialogAction::None => {}
            }
        } else {
            // Running phase — same veil/dialog dressing, but a
            // progress bar + status line; closes itself on success,
            // shows the error + Close on failure.
            let (stage, frac, done) = emu
                .upd_job
                .as_ref()
                .map(|j| {
                    let p = j.lock().unwrap_or_else(|e| e.into_inner());
                    (p.stage.clone(), p.frac, p.done.clone())
                })
                .unwrap_or_default();
            let name = handler_spec(&upd_key).map(|sp| sp.name).unwrap_or(upd_key.as_str());
            match upd_progress_overlay(
                ui,
                content,
                &format!("Updating {name}"),
                &stage,
                frac,
                done.as_ref().is_some_and(|d| d.is_ok()),
                done.as_ref().and_then(|d| d.as_ref().err().map(|e| e.as_str())),
                s,
                emu.upd_vis,
            ) {
                UpdPhase::Running => {}
                UpdPhase::Closed => {
                    // Success — fade out; a failure's Close lands here
                    // too, keeping the band icon so it can be retried.
                    let failed = done.as_ref().is_some_and(|d| d.is_err());
                    emu.upd_open = false;
                    emu.upd_job = None;
                    if failed {
                        emu.upd_skipped.insert(upd_key, upd_subtitle);
                    }
                }
            }
            ui.ctx().request_repaint();
        }
        ui.ctx().request_repaint();
    }
}

/// The update prompt's running phase.
enum UpdPhase {
    /// Still downloading/installing — keep the dialog up.
    Running,
    /// Finished and acknowledged (auto on success, Close on error).
    Closed,
}

/// Progress view for a running emulator update — same veil + scaled
/// dialog dressing as `confirm_overlay`, but a status line and a
/// progress bar instead of buttons. Success closes on its own; a
/// failure swaps the bar for the error + a Close button.
fn upd_progress_overlay(
    ui: &mut egui::Ui,
    rect: Rect,
    title: &str,
    stage: &str,
    frac: f32,
    succeeded: bool,
    err: Option<&str>,
    s: f32,
    vis: f32,
) -> UpdPhase {
    let painter = ui.painter().clone();
    painter.rect_filled(
        rect,
        CornerRadius::ZERO,
        Color32::from_black_alpha((170.0 * vis) as u8),
    );
    let pop = 0.94 + 0.06 * vis;
    let dlg = Rect::from_center_size(rect.center(), Vec2::new(420.0 * s, 170.0 * s) * pop);
    let dr = CornerRadius::same((14.0 * s).round() as u8);
    painter.rect_filled(dlg, dr, Color32::from_black_alpha((240.0 * vis) as u8));
    painter.rect_stroke(
        dlg,
        dr,
        Stroke::new(1.0, Color32::from_white_alpha((70.0 * vis) as u8)),
        StrokeKind::Inside,
    );
    painter.text(
        Pos2::new(dlg.center().x, dlg.min.y + 36.0 * s * pop),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(20.0 * s),
        Color32::WHITE.gamma_multiply(vis),
    );
    painter.text(
        Pos2::new(dlg.center().x, dlg.min.y + 66.0 * s * pop),
        Align2::CENTER_CENTER,
        if err.is_some() { "Update failed" } else { stage },
        FontId::proportional(13.0 * s),
        Color32::from_gray(190).gamma_multiply(vis),
    );
    if let Some(err) = err {
        // Failure — clipped error line above a centered Close button.
        let clip = Rect::from_min_max(
            Pos2::new(dlg.min.x + 20.0 * s, dlg.min.y + 82.0 * s),
            Pos2::new(dlg.max.x - 20.0 * s, dlg.min.y + 104.0 * s),
        );
        painter.with_clip_rect(clip).text(
            clip.center(),
            Align2::CENTER_CENTER,
            err,
            FontId::proportional(11.0 * s),
            Color32::from_rgb(0xE0, 0x70, 0x70).gamma_multiply(vis),
        );
        let b = Rect::from_center_size(
            Pos2::new(dlg.center().x, dlg.min.y + 126.0 * s * pop),
            Vec2::new(140.0 * s, 38.0 * s),
        );
        let resp = ui.interact(b, ui.id().with("emu-upd-close"), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let br = CornerRadius::same((8.0 * s).round() as u8);
        painter.rect_filled(
            b,
            br,
            Color32::from_gray(if resp.hovered() { 70 } else { 50 }).gamma_multiply(vis),
        );
        painter.rect_stroke(
            b,
            br,
            Stroke::new(1.0, Color32::from_white_alpha((120.0 * vis) as u8)),
            StrokeKind::Inside,
        );
        painter.text(
            b.center(),
            Align2::CENTER_CENTER,
            "Close",
            FontId::proportional(15.0 * s),
            Color32::WHITE.gamma_multiply(vis),
        );
        return if resp.clicked() { UpdPhase::Closed } else { UpdPhase::Running };
    }
    if succeeded {
        return UpdPhase::Closed;
    }
    // Progress bar — same track/fill styling as the settings card's.
    let track = Rect::from_min_size(
        Pos2::new(dlg.min.x + 40.0 * s, dlg.min.y + 118.0 * s * pop),
        Vec2::new(dlg.width() - 80.0 * s, 8.0 * s),
    );
    painter.rect_filled(track, CornerRadius::same(4), Color32::from_gray(50).gamma_multiply(vis));
    painter.rect_filled(
        Rect::from_min_size(track.min, Vec2::new(track.width() * frac.clamp(0.0, 1.0), track.height())),
        CornerRadius::same(4),
        Color32::from_rgb(0x1E, 0x6B, 0x3A).gamma_multiply(vis),
    );
    UpdPhase::Running
}

/// Overlay header title — the resolved/custom title when present,
/// else the cleaned file stem.
fn rom_title(p: &Path) -> String {
    emu_meta::cached_title(p)
        // A cached 3DS product code is a stale scan artifact — the
        // resolver heals it, but headers shouldn't flash it meanwhile.
        .filter(|t| !emu_meta::looks_like_3ds_product_code(t))
        .or_else(|| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| emu_meta::display_title(s))
        })
        .unwrap_or_else(|| p.display().to_string())
}

/// Resolve the Xenia storage root for the overlays — the configured
/// launcher's directory first (or a managed install when unset).
fn xenia_storage(cfg: &settings::Emulation) -> Option<PathBuf> {
    let configured = cfg
        .emulators
        .get(XENIA_HANDLER)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let launcher = configured.filter(|p| p.exists()).or_else(emu_dl::xenia_exe);
    xenia_cfg::storage_root(launcher.as_deref().and_then(|p| p.parent()))
}

/// Resolve the RPCS3 emulator directory for the patches overlay —
/// Lacrima's `ResolveEmulatorDirectory`: prefer the AES-managed
/// install when it already holds the patch DB, else the configured
/// launcher's directory, else the managed dir anyway.
fn rpcs3_storage(cfg: &settings::Emulation) -> Option<PathBuf> {
    let managed = emu_dl::rpcs3_dir();
    let configured = cfg
        .emulators
        .get(RPCS3_HANDLER)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let managed_has_db = managed
        .as_ref()
        .is_some_and(|m| m.join("patches").join("patch.yml").is_file());
    if managed_has_db {
        return managed;
    }
    configured.or(managed)
}

/// Resolve the shadPS4 emulator directory — the dir holding the exe
/// (`user/` under it carries patches/cheats). Lacrima's
/// ResolveShadPs4EmulatorDirectory semantics: the configured
/// launcher's dir first (upgraded off qtlauncher via
/// resolve_exe_path), else the managed install.
fn shadps4_storage(cfg: &settings::Emulation) -> Option<PathBuf> {
    let configured = cfg
        .emulators
        .get(SHADPS4_HANDLER)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .map(|p| shadps4::resolve_exe_path(&p));
    let exe = configured.or_else(emu_dl::shadps4_exe);
    exe.and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Resolve the Cemu emulator directory for the graphic-packs overlay
/// — `CemuPathsService.ResolveEmulatorDirectory`: the configured
/// launcher's directory first, else the managed install.
fn cemu_storage(cfg: &settings::Emulation) -> Option<PathBuf> {
    let configured = cfg
        .emulators
        .get(CEMU_HANDLER)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    configured.or_else(|| emu_dl::cemu_dir().filter(|d| d.is_dir()))
}

/// Destination rect + UV for a captured frame in `rect`, blended by
/// `crop_t`: 0 = pure aspect-fit, 1 = aspect-fill covering `rect`.
/// The UV is NOT blended — it's the centre-crop matching `dest`'s
/// actual aspect, so the sampled region and the draw rect always
/// agree: zero distortion at every point of the transition (blending
/// a fixed-shape uv against an animating dest squashes the frame —
/// the "jumping" artifact).
///
/// `stretch` is Lacrima's SelectedStretch: 0 Uniform (aspect-fit),
/// 1 UniformToFill (crop to cover — crop_t pinned to 1 by the
/// caller), 2 Fill (stretch to `rect`, aspect ignored). `pillar` is
/// RemoveArcadePillarboxBars: measured left/right black-bar insets
/// in frame px (0,0 when off or none detected) — the sampled window
/// shrinks to the content between them.
fn frame_dest_uv(
    rect: Rect,
    fw: usize,
    fh: usize,
    crop_t: f32,
    stretch: u8,
    pillar: (f32, f32),
) -> (Rect, Rect) {
    let (pl, pr) = pillar;
    let fwf = fw as f32;
    let fhf = fh as f32;
    let cw = (fwf - pl - pr).max(1.0);
    let eff = cw / fhf; // content aspect
                        // Pillar window in uv: [wl, wr] horizontally, full height.
    let wl = (pl / fwf).clamp(0.0, 0.5);
    let wr = (1.0 - pr / fwf).clamp(0.5, 1.0);
    let ww = wr - wl;
    let cx = (wl + wr) * 0.5;
    if stretch == 2 {
        // Fill — the frame covers `rect`, aspect ignored (the uv
        // still honours the pillar crop so it doesn't re-appear).
        let uv = Rect::from_min_max(Pos2::new(wl, 0.0), Pos2::new(wr, 1.0));
        return (rect, uv);
    }
    let fit = (rect.width() / (fhf * eff)).min(rect.height() / fhf);
    let fit_dest = Rect::from_center_size(rect.center(), Vec2::new(fhf * eff * fit, fhf * fit));
    let dest = lerp_rect(fit_dest, rect, crop_t);
    let d = dest.width() / dest.height();
    let uw = (d / eff).min(1.0) * ww;
    let uh = (eff / d).min(1.0);
    let uv = Rect::from_min_max(
        Pos2::new(cx - uw / 2.0, 0.5 - uh / 2.0),
        Pos2::new(cx + uw / 2.0, 0.5 + uh / 2.0),
    );
    (dest, uv)
}

/// The input tunnel — the emulator window is parked invisible and
/// offscreen, so focus-based input can't reach it. While
/// `input_tun` is on, every pointer/key event over the frame rect is
/// re-mapped through the same dest→uv transform the draw used and
/// posted into the emulator's WndProc (client coords), so its own
/// UI — sign-in dialogs, menus, text fields — is fully usable in
/// the captured stream. LMB/MMB/wheel/keys forward; RMB keeps the
/// app context menu and Esc releases the tunnel (it never reaches
/// the game — see the Esc chain in main.rs).
#[cfg(windows)]
fn tunnel_input(ui: &egui::Ui, emu: &mut EmuView, active: bool) {
    if !emu.input_tun || !active {
        return;
    }
    let Some((dest, uv)) = emu.tun_map else {
        emu.input_tun = false;
        return;
    };
    let Some(session) = &emu.session else {
        emu.input_tun = false;
        return;
    };
    if !matches!(session.status(), xenia::Status::Capturing) {
        return;
    }
    let Some((cw, ch)) = session.client_size() else {
        emu.tun_dbg = Some("no client rect".into());
        return;
    };
    emu.tun_dbg = None;
    // egui point → emulator client pixel. `uv` is the sampled frame
    // window (pillar crops included) normalized 0..1, so a fraction
    // of the client size lands on the right texel without needing
    // the frame's own DPI scale.
    // Callers gate `dest.contains` themselves; the map clamps so
    // drags/releases that leave the frame still resolve to an edge.
    let map = |p: Pos2| -> Option<(i32, i32)> {
        if dest.width() <= 0.0 || dest.height() <= 0.0 {
            return None;
        }
        let p = Pos2::new(
            p.x.clamp(dest.min.x, dest.max.x),
            p.y.clamp(dest.min.y, dest.max.y),
        );
        let tx = (p.x - dest.min.x) / dest.width();
        let ty = (p.y - dest.min.y) / dest.height();
        Some((
            ((uv.min.x + tx * uv.width()) * cw as f32).round() as i32,
            ((uv.min.y + ty * uv.height()) * ch as f32).round() as i32,
        ))
    };
    let btns = &mut emu.tun_buttons;
    let mods = &mut emu.tun_mods;
    let sent = &mut emu.tun_sent;
    ui.input(|i| {
        // MK_* wParam flags (stable Win32 values — MK_SHIFT=0x04,
        // MK_CONTROL=0x08; the held-button bits match MK_LBUTTON/
        // MK_RBUTTON/MK_MBUTTON numbering).
        let mk = |b: u8, m: egui::Modifiers| -> usize {
            let mut w = b as usize;
            if m.shift {
                w |= 0x04;
            }
            if m.ctrl {
                w |= 0x08;
            }
            w
        };
        for ev in &i.events {
            match ev {
                egui::Event::PointerMoved(p) => {
                    // Hover feeds the emulator's UI even with no
                    // buttons; while dragging, clamp so drags that
                    // leave the frame still track.
                    let in_dest = dest.contains(*p);
                    if in_dest || *btns != 0 {
                        if let Some((x, y)) = map(*p) {
                            *sent +=
                                session.post_at(WM_MOUSEMOVE, WPARAM(mk(*btns, i.modifiers)), x, y)
                                    as u32;
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    // RMB keeps the app's session menu — the tunnel
                    // forwards LMB/MMB only.
                    let (down, up, bit) = match button {
                        egui::PointerButton::Primary => (WM_LBUTTONDOWN, WM_LBUTTONUP, 0x01u8),
                        egui::PointerButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, 0x10u8),
                        _ => continue,
                    };
                    let held = *btns & bit != 0;
                    if *pressed {
                        *btns |= bit;
                    } else {
                        *btns &= !bit;
                    }
                    // Presses need a point inside the frame; releases
                    // always forward when the button was held so a
                    // drag-out can't leave it stuck.
                    if (*pressed && dest.contains(*pos)) || (!*pressed && held) {
                        if let Some((x, y)) = map(*pos) {
                            *sent += session.post_at(
                                if *pressed { down } else { up },
                                WPARAM(mk(*btns, *modifiers)),
                                x,
                                y,
                            ) as u32;
                        }
                    }
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } => {
                    if let Some(p) = i.pointer.latest_pos() {
                        if let Some((x, y)) = map(p) {
                            if !dest.contains(p) {
                                continue;
                            }
                            // Wheel deltas are in 1/120 units in
                            // wParam's high word; screen coords in
                            // lParam.
                            let d = match unit {
                                egui::MouseWheelUnit::Line => delta.y * 40.0,
                                egui::MouseWheelUnit::Page => delta.y * 480.0,
                                _ => delta.y * 3.0,
                            }
                            .clamp(i16::MIN as f32, i16::MAX as f32)
                                as i16;
                            if d != 0 {
                                let (sx, sy) = session.client_to_screen(x, y);
                                *sent += session.post(
                                    WM_MOUSEWHEEL,
                                    WPARAM(((d as u16) as usize) << 16 | mk(*btns, *modifiers)),
                                    LPARAM(sx as isize & 0xFFFF | (sy as isize) << 16),
                                ) as u32;
                            }
                        }
                    }
                }
                egui::Event::Key {
                    key,
                    pressed,
                    repeat,
                    modifiers,
                    ..
                } => {
                    // Esc is the app's release key — never forwarded.
                    if *key == egui::Key::Escape {
                        continue;
                    }
                    let Some(vk) = xenia::key_vk(*key) else {
                        continue;
                    };
                    xenia::sync_mods(session, modifiers, mods);
                    *sent += session.post(
                        if *pressed { WM_KEYDOWN } else { WM_KEYUP },
                        WPARAM(vk as usize),
                        xenia::key_lparam(vk, *pressed, *repeat),
                    ) as u32;
                }
                egui::Event::Text(t) => {
                    for cu in t.encode_utf16() {
                        *sent += session.post(WM_CHAR, WPARAM(cu as usize), LPARAM(1)) as u32;
                    }
                }
                egui::Event::ModifiersChanged(m) => {
                    xenia::sync_mods(session, m, mods);
                }
                _ => {}
            }
        }
    });
    // While the game owns input the app's live shortcuts must not
    // also fire — Backspace would navigate back mid-game, F12 would
    // toggle fullscreen, Ctrl+F opens search, Ctrl+arrows seek the
    // player. They run after this view, so consuming here preempts
    // them; Esc is deliberately left — it releases the tunnel.
    ui.ctx().input_mut(|i| {
        i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace);
        i.consume_key(egui::Modifiers::NONE, egui::Key::F12);
        i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
        for k in [
            egui::Key::ArrowUp,
            egui::Key::ArrowDown,
            egui::Key::ArrowLeft,
            egui::Key::ArrowRight,
        ] {
            i.consume_key(egui::Modifiers::CTRL, k);
            i.consume_key(egui::Modifiers::NONE, k);
        }
    });
}

/// Linux input path — the emulator lives inside the nested compositor
/// (`Session::embedded`), so egui events translate to evdev codes and
/// ride the compositor's wl_seat straight to the client's surface.
/// Same toggle/geometry as the Windows PostMessage tunnel; the big
/// differences are evdev codes instead of VKs, axis values instead of
/// WM_MOUSEWHEEL deltas, and modifiers — egui reports them only via
/// ModifiersChanged, so their key events are synthesized on edges
/// (smithay's xkb updates and forwards wl_keyboard.modifiers itself).
#[cfg(not(windows))]
fn tunnel_input(ui: &egui::Ui, emu: &mut EmuView, active: bool) {
    use crate::emulation::wlcomp::WlInput;
    if !emu.input_tun || !active {
        return;
    }
    let Some((dest, uv)) = emu.tun_map else {
        emu.input_tun = false;
        return;
    };
    let Some(session) = &emu.session else {
        emu.input_tun = false;
        return;
    };
    if !session.embedded() || !matches!(session.status(), xenia::Status::Capturing) {
        return;
    }
    // Surface-local coords equal captured pixels at buffer scale 1 —
    // the frame texture size is the client coordinate space.
    let Some((cw, ch)) = emu
        .frame_tex
        .as_ref()
        .map(|t| (t.size()[0] as f64, t.size()[1] as f64))
    else {
        emu.tun_dbg = Some("no frame yet".into());
        return;
    };
    emu.tun_dbg = None;
    let map = |p: Pos2| -> Option<(f64, f64)> {
        if dest.width() <= 0.0 || dest.height() <= 0.0 {
            return None;
        }
        let p = Pos2::new(
            p.x.clamp(dest.min.x, dest.max.x),
            p.y.clamp(dest.min.y, dest.max.y),
        );
        let tx = (p.x - dest.min.x) / dest.width();
        let ty = (p.y - dest.min.y) / dest.height();
        Some((
            (uv.min.x + tx * uv.width()) as f64 * cw,
            (uv.min.y + ty * uv.height()) as f64 * ch,
        ))
    };
    let btns = &mut emu.tun_buttons;
    let mods = &mut emu.tun_mods;
    let sent = &mut emu.tun_sent;
    // Synthesized modifier keys — ModifiersChanged only carries the
    // *new* state, so edges are detected against our shadow bitmask.
    // Bit layout mirrors the Windows tunnel (bit0 shift, bit1 ctrl,
    // bit2 alt, bit3 super).
    let send_mod = |session: &xenia::Session, mods: &mut u8, bit: u8, down: bool, code: u32| {
        let held = *mods & bit != 0;
        if down != held {
            if down {
                *mods |= bit;
            } else {
                *mods &= !bit;
            }
            session.send_input(WlInput::Key {
                code,
                pressed: down,
            });
        }
    };
    ui.input(|i| {
        for ev in &i.events {
            match ev {
                egui::Event::PointerMoved(p) => {
                    if dest.contains(*p) || *btns != 0 {
                        if let Some((x, y)) = map(*p) {
                            session.send_input(WlInput::PointerMotion { x, y });
                            *sent += 1;
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    // RMB keeps the app's session menu — LMB/MMB/
                    // side-buttons forward as BTN_LEFT/MIDDLE/etc.
                    let (code, bit) = match button {
                        egui::PointerButton::Primary => (0x110, 0x01u8), // BTN_LEFT
                        egui::PointerButton::Middle => (0x112, 0x02u8), // BTN_MIDDLE
                        egui::PointerButton::Extra1 => (0x114, 0x04u8), // BTN_SIDE
                        egui::PointerButton::Extra2 => (0x115, 0x08u8), // BTN_EXTRA
                        _ => continue,
                    };
                    let held = *btns & bit != 0;
                    if *pressed {
                        *btns |= bit;
                    } else {
                        *btns &= !bit;
                    }
                    if (*pressed && dest.contains(*pos)) || (!*pressed && held) {
                        if map(*pos).is_some() {
                            session.send_input(WlInput::PointerButton {
                                button: code,
                                pressed: *pressed,
                            });
                            *sent += 1;
                        }
                    }
                }
                egui::Event::MouseWheel { delta, .. } => {
                    if let Some(p) = i.pointer.latest_pos() {
                        if dest.contains(p) {
                            session.send_input(WlInput::PointerAxis {
                                horizontal: delta.x as f64 * 15.0,
                                vertical: delta.y as f64 * 15.0,
                            });
                            *sent += 1;
                        }
                    }
                }
                egui::Event::Key {
                    key, pressed, ..
                } => {
                    // Esc is the app's release key — never forwarded.
                    if *key == egui::Key::Escape {
                        continue;
                    }
                    if let Some(code) = key_evdev(*key) {
                        session.send_input(WlInput::Key {
                            code,
                            pressed: *pressed,
                        });
                        *sent += 1;
                    }
                }
                egui::Event::ModifiersChanged(m) => {
                    send_mod(session, mods, 0x01, m.shift, 42); // KEY_LEFTSHIFT
                    send_mod(session, mods, 0x02, m.ctrl, 29); // KEY_LEFTCTRL
                    send_mod(session, mods, 0x04, m.alt, 56); // KEY_LEFTALT
                    send_mod(session, mods, 0x08, m.mac_cmd || m.command, 125); // KEY_LEFTMETA
                }
                // Text lands through the keycodes — wl_keyboard has no
                // char event; the client's xkb composes it.
                _ => {}
            }
        }
    });
    // Same shortcut suppression as the Windows tunnel — while the
    // emulator owns input, Backspace/F12/Ctrl+F/arrows must not fire
    // app shortcuts underneath. Esc stays — it releases the tunnel.
    ui.ctx().input_mut(|i| {
        i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace);
        i.consume_key(egui::Modifiers::NONE, egui::Key::F12);
        i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
        for k in [
            egui::Key::ArrowUp,
            egui::Key::ArrowDown,
            egui::Key::ArrowLeft,
            egui::Key::ArrowRight,
        ] {
            i.consume_key(egui::Modifiers::CTRL, k);
            i.consume_key(egui::Modifiers::NONE, k);
        }
    });
}

/// egui Key → evdev keycode (linux/input-event-codes.h). Named char
/// keys map to the physical key that produces them unshifted —
/// clients re-derive the shifted char through their own xkb state.
#[cfg(not(windows))]
fn key_evdev(key: egui::Key) -> Option<u32> {
    Some(match key {
        egui::Key::Escape => 1,
        egui::Key::Num1 | egui::Key::Exclamationmark => 2,
        egui::Key::Num2 => 3,
        egui::Key::Num3 => 4,
        egui::Key::Num4 => 5,
        egui::Key::Num5 => 6,
        egui::Key::Num6 => 7,
        egui::Key::Num7 => 8,
        egui::Key::Num8 => 9,
        egui::Key::Num9 => 10,
        egui::Key::Num0 => 11,
        egui::Key::Minus => 12,
        egui::Key::Equals | egui::Key::Plus => 13,
        egui::Key::Backspace => 14,
        egui::Key::Tab => 15,
        egui::Key::Q => 16,
        egui::Key::W => 17,
        egui::Key::E => 18,
        egui::Key::R => 19,
        egui::Key::T => 20,
        egui::Key::Y => 21,
        egui::Key::U => 22,
        egui::Key::I => 23,
        egui::Key::O => 24,
        egui::Key::P => 25,
        egui::Key::OpenBracket | egui::Key::OpenCurlyBracket => 26,
        egui::Key::CloseBracket | egui::Key::CloseCurlyBracket => 27,
        egui::Key::Enter => 28,
        egui::Key::A => 30,
        egui::Key::S => 31,
        egui::Key::D => 32,
        egui::Key::F => 33,
        egui::Key::G => 34,
        egui::Key::H => 35,
        egui::Key::J => 36,
        egui::Key::K => 37,
        egui::Key::L => 38,
        egui::Key::Semicolon | egui::Key::Colon => 39,
        egui::Key::Quote => 40,
        egui::Key::Backtick => 41,
        egui::Key::Backslash | egui::Key::Pipe => 43,
        egui::Key::Z => 44,
        egui::Key::X => 45,
        egui::Key::C => 46,
        egui::Key::V => 47,
        egui::Key::B => 48,
        egui::Key::N => 49,
        egui::Key::M => 50,
        egui::Key::Comma => 51,
        egui::Key::Period => 52,
        egui::Key::Slash | egui::Key::Questionmark => 53,
        egui::Key::Space => 57,
        egui::Key::F1 => 59,
        egui::Key::F2 => 60,
        egui::Key::F3 => 61,
        egui::Key::F4 => 62,
        egui::Key::F5 => 63,
        egui::Key::F6 => 64,
        egui::Key::F7 => 65,
        egui::Key::F8 => 66,
        egui::Key::F9 => 67,
        egui::Key::F10 => 68,
        egui::Key::F11 => 87,
        egui::Key::F12 => 88,
        egui::Key::F13 => 183,
        egui::Key::F14 => 184,
        egui::Key::F15 => 185,
        egui::Key::F16 => 186,
        egui::Key::F17 => 187,
        egui::Key::F18 => 188,
        egui::Key::F19 => 189,
        egui::Key::F20 => 190,
        egui::Key::F21 => 191,
        egui::Key::F22 => 192,
        egui::Key::F23 => 193,
        egui::Key::F24 => 194,
        egui::Key::Home => 102,
        egui::Key::ArrowUp => 103,
        egui::Key::PageUp => 104,
        egui::Key::ArrowLeft => 105,
        egui::Key::ArrowRight => 106,
        egui::Key::End => 107,
        egui::Key::ArrowDown => 108,
        egui::Key::PageDown => 109,
        egui::Key::Insert => 110,
        egui::Key::Delete => 111,
        _ => return None,
    })
}

/// Live render adjustments for the zero-copy path — the GPU-side
/// shader/passthrough applies these since no CPU swizzle ran.
fn render_adjust(emu: &EmuView) -> (f32, f32, [f32; 4]) {
    let [r, g, b] = emu.render.tint;
    (
        emu.render.brightness,
        emu.render.saturation,
        [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0],
    )
}

/// SelectedCaptureAspectRatioKey → ratio — "handler" (or anything
/// unrecognized) means the emulator's own, no resize.
#[cfg(windows)]
fn aspect_ratio_of(key: &str) -> Option<f64> {
    match key {
        "4:3" => Some(4.0 / 3.0),
        "16:9" => Some(16.0 / 9.0),
        "21:9" => Some(21.0 / 9.0),
        "32:9" => Some(32.0 / 9.0),
        "1:1" => Some(1.0),
        _ => None,
    }
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Rect::from_min_max(
        Pos2::new(l(a.min.x, b.min.x), l(a.min.y, b.min.y)),
        Pos2::new(l(a.max.x, b.max.x), l(a.max.y, b.max.y)),
    )
}

/// The capture presentation layer (Lacrima's
/// EmulatorCapturePresentationLayer): an opaque black backing + the
/// latest WGC frame aspect-fit + a centered status line while the
/// render window resolves. The whole layer paints at `alpha` — the
/// caller eases it for the list ⇄ game fade. No labels or search
/// chrome draw on top of a live capture.
fn draw_session(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    target: Rect,
    emu: &mut EmuView,
    alpha: f32,
    s: f32,
    letterbox_fill: bool,
) {
    // Opaque backing — never a compositor hole, matching the
    // #FF000000 Border behind Lacrima's capture host.
    painter.rect_filled(rect, 0.0, Color32::BLACK.gamma_multiply(alpha));

    // Back-cover letterbox fill (DrawLetterboxBackground): the ROM's
    // "Back Cover" `.meta` payload — bar-cropped like the other web
    // art — UniformToFill'd under the frame so pillarbox/letterbox
    // area shows art. Kept through the fade-out once loaded.
    if letterbox_fill {
        if let Some(game) = emu.session.as_ref().map(|s| s.game.clone()) {
            if emu.letterbox_key.as_deref() != Some(game.as_path()) {
                emu.letterbox_key = Some(game);
                emu.letterbox_tex = emu
                    .letterbox_key
                    .as_deref()
                    .and_then(emu_meta::back_cover_path)
                    // `image::open` keys the decoder off the
                    // extension — `.k0` payloads need content
                    // sniffing (load_from_memory) instead.
                    .and_then(|p| std::fs::read(&p).ok())
                    .and_then(|b| image::load_from_memory(&b).ok())
                    .map(|img| {
                        let rgba = library::crop_black_bars(img).to_rgba8();
                        let (w, h) = rgba.dimensions();
                        ui.ctx().load_texture(
                            "emu-letterbox",
                            egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &rgba,
                            ),
                            egui::TextureOptions::LINEAR,
                        )
                    });
            }
        }
        if let Some(tex) = &emu.letterbox_tex {
            let uv = crate::tag_editor::cover_uv(
                Vec2::new(tex.size()[0] as f32, tex.size()[1] as f32),
                rect.size(),
            );
            painter.with_clip_rect(rect).image(
                tex.id(),
                rect,
                uv,
                Color32::WHITE.gamma_multiply(alpha),
            );
        }
    }
    // Lacrima's IsBackCoverLetterboxBackgroundActive: the fill only
    // shows where the frame's baked-in bars are cropped away, so an
    // active fill implies pillarbox detection even when "Remove side
    // pillars" is off (its effectiveEnableAutoCrop).
    let pillar_active =
        emu.render.remove_pillars || (letterbox_fill && emu.letterbox_tex.is_some());

    let Some(session) = &emu.session else {
        emu.tun_map = None;
        // Session ended — the last frame (if any) still fades out.
        // The imported GPU ring stays alive (its wgpu textures hold
        // the shared allocation) so the zero-copy path fades too.
        let (fw, fh) = if let Some(g) = &emu.gpu_cap {
            (g.size[0] as usize, g.size[1] as usize)
        } else if let Some(t) = &emu.frame_tex {
            (t.size()[0], t.size()[1])
        } else {
            return;
        };
        let (dest, uv) =
            frame_dest_uv(rect, fw, fh, emu.crop_t, emu.render.stretch, emu.pillar_cur);
        let seq = emu.frame_seq;
        let adjust = render_adjust(emu);
        let cb = emu
            .gpu_cap
            .as_ref()
            .map(|gc| {
                (
                    gc.bgs[(seq % 3) as usize].clone(),
                    gc.fence.clone(),
                    gc.size,
                )
            })
            .and_then(|(bg, fence, size)| {
                emu.cap_shader.as_mut().and_then(|cs| {
                    cs.callback_gpu(
                        &emu.render.shader,
                        dest,
                        uv,
                        alpha,
                        size,
                        seq as f32,
                        ui.ctx().pixels_per_point(),
                        bg,
                        // The writer is gone — the fence was already
                        // signaled, but the wait is cheap + strictly
                        // correct cross-API ordering.
                        Some((fence, seq)),
                        adjust,
                    )
                })
            });
        match cb {
            Some(cb) => {
                painter.add(eframe::egui_wgpu::Callback::new_paint_callback(dest, cb));
            }
            None => {
                if let Some(tex) = &emu.frame_tex {
                    painter.image(tex.id(), dest, uv, Color32::WHITE.gamma_multiply(alpha));
                }
            }
        }
        return;
    };
    let status = session.status();
    let game_name = session
        .game
        .file_stem()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "game".into());

    // Zero-copy import — the capture publishes a new ring generation
    // on every size change; a failed import pins the session to the
    // CPU mirror until a new generation appears. The capture side's
    // own failure (no ring at all) surfaces the same way.
    if emu.gpu_err.is_none() {
        if let Some(e) = session
            .gpu_state()
            .err
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            emu.gpu_err = Some(e);
        }
    }
    if emu.cap_shader.is_some() {
        let share = session
            .gpu_state()
            .share
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(share) = share {
            if emu.gpu_cap.as_ref().map(|g| g.generation) != Some(share.generation)
                && emu.gpu_failed_gen != share.generation
            {
                match emu.cap_shader.as_mut().map(|cs| cs.import_share(&share)) {
                    Some(Ok(r)) => {
                        emu.gpu_cap = Some(r);
                        emu.gpu_failed_gen = 0;
                        emu.gpu_err = None;
                    }
                    Some(Err(e)) => {
                        emu.gpu_failed_gen = share.generation;
                        emu.gpu_err = Some(e);
                    }
                    None => {}
                }
            }
        }
    }
    // The capture's CPU readback only earns its keep when something
    // actually needs pixels there — display on the mirror path, or
    // pillarbox detection.
    session
        .render()
        .want_cpu
        .store(pillar_active || emu.gpu_cap.is_none(), Ordering::Relaxed);

    // Pull the newest captured frame — on the zero-copy path only the
    // ring seq matters (stats + shader FrameCount); on the CPU mirror
    // path the slot's pixels are swapped in and uploaded. Skipped
    // while the layer is fully faded out so hidden sessions don't
    // burn uploads.
    if alpha > 0.02 {
        let gpu_live = emu.gpu_cap.is_some();
        let mut new_seq: Option<u64> = None;
        // The CPU slot is consumed when the display needs its pixels
        // (mirror path) or pillarbox detection does.
        if !gpu_live || pillar_active {
            // Swap the frame into `frame_buf` — the slot gets the old
            // buffer back so the capture thread's scratch<->slot
            // ping-pong stays allocation-free.
            let mut slot = session.frames().lock().unwrap_or_else(|p| p.into_inner());
            if slot.seq != emu.slot_seq
                && slot.w > 0
                && slot.h > 0
                && slot.rgba.len() == slot.w * slot.h * 4
            {
                let (w, h) = (slot.w, slot.h);
                std::mem::swap(&mut slot.rgba, &mut emu.frame_buf);
                let seq = slot.seq;
                drop(slot);
                emu.slot_seq = seq;
                if !gpu_live {
                    let img = egui::ColorImage {
                        size: [w, h],
                        source_size: Vec2::new(w as f32, h as f32),
                        // Opaque frames — premult == unmult.
                        pixels: bytemuck::cast_slice::<u8, egui::Color32>(&emu.frame_buf).to_vec(),
                    };
                    match &mut emu.frame_tex {
                        Some(tex) if tex.size() == [w, h] => {
                            tex.set(img, egui::TextureOptions::LINEAR)
                        }
                        _ => {
                            emu.frame_tex = Some(ui.ctx().load_texture(
                                "emu-capture",
                                img,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                    // The GPU shader path samples its own texture —
                    // upload the same frame straight from the staging
                    // buffer (dedup'd by seq inside).
                    if !emu.render.shader.is_empty() {
                        if let Some(cs) = &mut emu.cap_shader {
                            cs.upload_frame(seq, &emu.frame_buf, w as u32, h as u32);
                        }
                    }
                    new_seq = Some(seq);
                }
                // Remove-side-pillars: a locked per-ROM crop wins
                // outright (Lacrima's EnforceLockedArcadeCrop); otherwise
                // measure the frame's black bars (PillarboxBarDetector,
                // aggressive profile since AggressivePillarboxCrop ==
                // RemoveArcadePillarboxBars there) + the arcade resolver,
                // then the 6-stable-frame debounce before retargeting.
                if pillar_active {
                    let det = if let Some((ll, lr, lfw)) = emu.pillar_lock {
                        crate::emulation::pillarbox::scale_locked(ll, lr, lfw, w as u32)
                    } else {
                        crate::emulation::pillarbox::resolve(
                            w as u32,
                            crate::emulation::pillarbox::detect(
                                &emu.frame_buf,
                                w,
                                h,
                                crate::emulation::pillarbox::Profile::Aggressive,
                            ),
                        )
                    };
                    let locked = emu.pillar_lock.is_some();
                    if det == emu.pillar_last {
                        emu.pillar_stable = emu.pillar_stable.saturating_add(1).min(60);
                    } else {
                        emu.pillar_last = det;
                        emu.pillar_stable = 1;
                    }
                    // Locked crops enforce immediately — detection
                    // doesn't run on a locked resolver either.
                    if (locked || emu.pillar_stable >= 6) && det != emu.pillar_tgt {
                        emu.pillar_tgt = det;
                        emu.pillar_from = emu.pillar_cur;
                        emu.pillar_prog = 0.0;
                    }
                }
            }
        }
        // Zero-copy path — the ring's signaled seq IS the frame
        // counter (and the fence value to wait on before sampling).
        if gpu_live {
            let seq = session.gpu_state().seq.load(Ordering::Acquire);
            if seq != emu.frame_seq {
                new_seq = Some(seq);
            }
        }
        if let Some(seq) = new_seq {
            emu.frame_seq = seq;
            // Stats-overlay bookkeeping — capture-side fps and the
            // rolling frametime ring (Lacrima's PortalCaptureFps /
            // PortalFrametimeGraphGeometry).
            let now = std::time::Instant::now();
            if let Some(last) = emu.stats_instant.replace(now) {
                let ms = now.duration_since(last).as_secs_f32() * 1000.0;
                emu.frametimes.push_back(ms);
                while emu.frametimes.len() > 240 {
                    emu.frametimes.pop_front();
                }
                emu.stats_ft_ms += (ms - emu.stats_ft_ms) * 0.15;
                emu.stats_fps += (1000.0 / ms.max(0.01) - emu.stats_fps) * 0.15;
            }
        }
    }

    // Ease the aspect-fill blend — the crop/letterbox glides instead
    // of snapping when the destination aspect crosses the ~12% fill
    // threshold (strip/band resizes, fullscreen transitions).
    let frame_dims = if let Some(g) = &emu.gpu_cap {
        Some((g.size[0] as usize, g.size[1] as usize))
    } else {
        emu.frame_tex.as_ref().map(|t| (t.size()[0], t.size()[1]))
    };
    let mut dbg_want_dt = None;
    if let Some((fw, fh)) = frame_dims {
        // The pillar crop replaces the frame's aspect with the
        // measured content's — the fill decision uses the debounced
        // TARGET insets so it can't flip mid-tween (crop_t bounce).
        let eff =
            ((fw as f32 - emu.pillar_tgt.0 as f32 - emu.pillar_tgt.1 as f32).max(1.0)) / fh as f32;
        // `want` uses the TARGET aspect, not the animating rect's —
        // otherwise it flips as the easing aspect sweeps through the
        // fill band mid-transition and crop_t reverses (the bounce).
        let dst = target.width() / target.height();
        let want = match emu.render.stretch {
            1 | 2 => 1.0, // UniformToFill / Fill always cover
            _ => ((eff / dst - 1.0).abs() < 0.12) as u8 as f32,
        };
        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
        // Tween the pillar insets toward the debounced target — off
        // tweens back to (0,0) so the bars slide back in (Lacrima's
        // PillarboxCropAnimator).
        if emu.pillar_prog < 1.0 {
            emu.pillar_prog = (emu.pillar_prog + dt / 0.3).min(1.0);
            let e = crate::ease_out_cubic(emu.pillar_prog);
            let (fl, fr) = emu.pillar_from;
            emu.pillar_cur = (
                fl + (emu.pillar_tgt.0 as f32 - fl) * e,
                fr + (emu.pillar_tgt.1 as f32 - fr) * e,
            );
            if emu.pillar_prog < 1.0 {
                ui.ctx().request_repaint();
            }
        }
        if !pillar_active && (emu.pillar_tgt != (0, 0) || emu.pillar_cur != (0.0, 0.0)) {
            emu.pillar_from = emu.pillar_cur;
            emu.pillar_tgt = (0, 0);
            emu.pillar_last = (0, 0);
            emu.pillar_stable = 0;
            emu.pillar_prog = 0.0;
        }
        // Fixed-duration cubic tween — same easing + duration as the
        // strip slide so the blend and the rect move in concert
        // (an exponential ease races ahead mid-transition and reads
        // as the two animations fighting).
        if want != emu.crop_target {
            emu.crop_from = emu.crop_t;
            emu.crop_target = want;
            emu.crop_prog = 0.0;
        }
        if emu.crop_prog < 1.0 {
            emu.crop_prog = (emu.crop_prog + dt / 0.55).min(1.0);
            let e = crate::ease_in_out_cubic(emu.crop_prog);
            emu.crop_t = emu.crop_from + (emu.crop_target - emu.crop_from) * e;
            if emu.crop_prog < 1.0 {
                ui.ctx().request_repaint();
            } else {
                emu.crop_t = emu.crop_target;
            }
        }
        dbg_want_dt = Some((want, dt));
    }

    if let Some((fw, fh)) = frame_dims {
        let (dest, uv) =
            frame_dest_uv(rect, fw, fh, emu.crop_t, emu.render.stretch, emu.pillar_cur);
        // The tunnel maps screen → frame → client through this same
        // transform — refresh it wherever the draw moved to.
        emu.tun_map = Some((dest, uv));
        // The zero-copy ring drives the draw when imported — the
        // newest signaled slot is sampled in place after the D3D12
        // queue waits on the cross-API fence. Otherwise the CPU
        // mirror path: named shader via PaintCallback or plain blit.
        let seq = emu.frame_seq;
        let adjust = render_adjust(emu);
        let cb = match emu.gpu_cap.as_ref().map(|gc| {
            (
                gc.bgs[(seq % 3) as usize].clone(),
                gc.fence.clone(),
                gc.size,
            )
        }) { Some((bg, fence, size)) => {
            emu.cap_shader.as_mut().and_then(|cs| {
                cs.callback_gpu(
                    &emu.render.shader,
                    dest,
                    uv,
                    alpha,
                    size,
                    seq as f32,
                    ui.ctx().pixels_per_point(),
                    bg,
                    Some((fence, seq)),
                    adjust,
                )
            })
        } _ => {
            emu.cap_shader.as_mut().and_then(|cs| {
                cs.callback(
                    &emu.render.shader,
                    dest,
                    uv,
                    alpha,
                    [fw as u32, fh as u32],
                    emu.frame_seq as f32,
                    ui.ctx().pixels_per_point(),
                )
            })
        }};
        match cb {
            Some(cb) => {
                // Must be the egui_wgpu::Callback wrapper — the
                // renderer downcasts the Arc to that exact type; a
                // bare PaintCallback shape is silently culled.
                painter.add(eframe::egui_wgpu::Callback::new_paint_callback(dest, cb));
            }
            None => {
                if let Some(tex) = &emu.frame_tex {
                    painter.image(tex.id(), dest, uv, Color32::WHITE.gamma_multiply(alpha));
                } else if emu.gpu_cap.is_some() {
                    // The interop path is live but its pipeline failed
                    // — surface it instead of a silent black frame.
                    emu.gpu_err = Some("capture pipeline build failed".to_string());
                }
            }
        }
        if let (Some(f), Some((want, dt))) = (&mut emu.motion_log, dbg_want_dt) {
            use std::io::Write as _;
            let t = emu.motion_t0.elapsed().as_secs_f32();
            let _ = writeln!(
                f,
                "t={t:.3} dt={:.1}ms rect={:.0}x{:.0} dest={:.0}x{:.0} crop={:.2} want={want}",
                dt * 1000.0,
                rect.width(),
                rect.height(),
                dest.width(),
                dest.height(),
                emu.crop_t,
            );
        }
    }

    // Centered status — the launch gate while resolving, failure/
    // exit notices; nothing overlays a live capture.
    let msg = match &status {
        xenia::Status::Resolving => Some(format!("Launching {game_name}…")),
        xenia::Status::Failed(e) => Some(format!("Capture failed: {e}")),
        xenia::Status::Exited => Some("Emulator exited".into()),
        xenia::Status::Capturing => {
            if cfg!(windows) {
                (emu.frame_seq == 0).then(|| format!("Waiting for frames — {game_name}"))
            } else if session.embedded() {
                (emu.frame_tex.is_none())
                    .then(|| format!("Waiting for frames — {game_name}"))
            } else {
                Some(format!("{game_name} is running in its own window"))
            }
        }
    };
    if let Some(msg) = msg {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(24.0 * s),
            Color32::from_gray(160).gamma_multiply(alpha),
        );
    }

    // Statistics overlay (Lacrima's PortalCaptureStatsOverlay) — a
    // bottom-left panel at RenderOverlayOpacity with capture fps /
    // frametime, optional GPU info, and the frametime graph.
    if emu.render.stats_overlay && matches!(session.status(), xenia::Status::Capturing) {
        draw_stats_overlay(painter, rect, emu, alpha, s);
    }

    // Input-tunnel state badge — bottom-right, doubles as the
    // diagnostic (the counter must rise on input; if it doesn't, our
    // side is dead; if it does, the emulator side ignores them).
    if emu.input_tun {
        let label = match &emu.tun_dbg {
            Some(e) => format!("INPUT → EMULATOR — {e}"),
            None => format!("INPUT → EMULATOR — {} msgs", emu.tun_sent),
        };
        let pos = Pos2::new(rect.max.x - 16.0 * s, rect.max.y - 16.0 * s);
        painter.text(
            pos,
            Align2::RIGHT_BOTTOM,
            label,
            FontId::proportional(15.0 * s),
            Color32::from_rgb(0x60, 0xD8, 0x80).gamma_multiply(alpha),
        );
    }
}

/// Lacrima's PortalCaptureStatsOverlay — a #C0000000 rounded panel
/// anchored bottom-left of the capture (Margin 16,16,16,24), whole
/// panel at RenderOverlayOpacity. Content: capture fps + frametime,
/// a status line, optional GPU renderer/vendor, and the 520x40
/// frametime polyline with 16.7/33.3 ms guides.
fn draw_stats_overlay(painter: &egui::Painter, rect: Rect, emu: &mut EmuView, alpha: f32, s: f32) {
    let op = (emu.render.overlay_opacity * alpha).clamp(0.0, 1.0);
    if op <= 0.01 {
        return;
    }
    let gpu = if emu.render.gpu_info {
        Some(
            emu.gpu_desc
                .get_or_insert_with(|| {
                    crate::emulation::wgc::gpu_desc()
                        .map(|(n, v)| format!("{n} ({v})"))
                        .unwrap_or_else(|| "Unknown".into())
                })
                .as_str(),
        )
    } else {
        None
    };
    let fid = FontId::monospace(15.0 * s);
    let line_h = 21.0 * s;
    let lines = 3 + gpu.is_some() as usize;
    let graph = emu.render.frametime_graph;
    let gw = 330.0 * s;
    let gh = 44.0 * s;
    let w = gw.max(200.0 * s) + 20.0 * s;
    let h = lines as f32 * line_h + 10.0 * s + if graph { gh + 10.0 * s } else { 0.0 };
    let panel = Rect::from_min_size(
        Pos2::new(rect.min.x + 16.0 * s, rect.max.y - 24.0 * s - h),
        Vec2::new(w, h),
    );
    painter.rect_filled(
        panel,
        CornerRadius::same((8.0 * s) as u8),
        Color32::from_black_alpha((192.0 * op) as u8),
    );
    let white = Color32::WHITE.gamma_multiply(op);
    let gray = Color32::from_gray(221).gamma_multiply(op);
    let amber = Color32::from_rgb(0xFF, 0xCC, 0x66).gamma_multiply(op);
    let mut y = panel.min.y + 5.0 * s + line_h * 0.5;
    let x = panel.min.x + 10.0 * s;
    let line = |painter: &egui::Painter, txt: String, c: Color32, y: &mut f32| {
        painter.text(Pos2::new(x, *y), Align2::LEFT_CENTER, txt, fid.clone(), c);
        *y += line_h;
    };
    line(
        painter,
        format!("Capture FPS: {:.1}", emu.stats_fps),
        white,
        &mut y,
    );
    line(
        painter,
        format!("Capture frame time: {:.2} ms", emu.stats_ft_ms),
        white,
        &mut y,
    );
    line(
        painter,
        if emu.gpu_cap.is_some() {
            "WGC — DX12 interop".to_string()
        } else if let Some(e) = &emu.gpu_err {
            format!("WGC — CPU capture ({e})")
        } else {
            "WGC — CPU capture".to_string()
        },
        amber,
        &mut y,
    );
    if let Some(g) = gpu {
        line(painter, format!("GPU: {g}"), gray, &mut y);
    }
    if graph && !emu.frametimes.is_empty() {
        let gr = Rect::from_min_size(Pos2::new(x, y + 4.0 * s), Vec2::new(gw, gh));
        // 16.7/33.3 ms guide lines (60/30 fps marks on a 0..50ms span).
        for ms in [16.7_f32, 33.3] {
            let gy = gr.max.y - (ms / 50.0).min(1.0) * gr.height();
            painter.hline(
                gr.min.x..=gr.max.x,
                gy,
                Stroke::new(1.0, Color32::from_white_alpha((40.0 * op) as u8)),
            );
        }
        let n = emu.frametimes.len();
        let ft: Vec<f32> = emu.frametimes.iter().copied().collect();
        // 2-tap smoothing — a raw per-frame polyline visibly
        // flickers; Lacrima's geometry is smoothed the same way.
        let sm = |i: usize| -> f32 {
            if i == 0 {
                ft[0]
            } else {
                (ft[i - 1] + ft[i]) * 0.5
            }
        };
        let pts: Vec<Pos2> = (0..n)
            .map(|i| {
                Pos2::new(
                    gr.min.x + gr.width() * i as f32 / (n.max(2) - 1) as f32,
                    gr.max.y - (sm(i) / 50.0).min(1.0) * gr.height(),
                )
            })
            .collect();
        painter.add(Shape::line(
            pts,
            Stroke::new(
                1.5 * s,
                Color32::from_rgb(0, 0xFF, 0xFF).gamma_multiply(op * 0.7),
            ),
        ));
    }
}

/// The item area with a section loaded and no session: the shared
/// `Carousel` widget in ROM mode — the audio view's exact list (the
/// band's layout icon drives Emulation.CoverLayoutMode, so carousel /
/// vertical / horizontal grids all work). Menus speak emulation:
/// Add Roms / Scan Folder / Play / Remove / Clear Section.
fn draw_items(
    ui: &mut egui::Ui,
    rect: Rect,
    emu: &mut EmuView,
    cfg: &mut settings::Emulation,
    car_cfg: &settings::Carousel,
    cover_layout: u8,
    alpha: f32,
    interactive: bool,
    s: f32,
) {
    let key = emu.loaded_key.clone().unwrap_or_default();
    // The Xenia item-menu entries only make sense for the X360
    // section wired to the xenia handler.
    emu.cards.xenia_menu =
        key == XENIA_SECTION && emu.active_handler(cfg).as_deref() == Some(XENIA_HANDLER);
    emu.cards.rpcs3_menu =
        key == PS3_SECTION && emu.active_handler(cfg).as_deref() == Some(RPCS3_HANDLER);
    emu.cards.shadps4_menu =
        key == PS4_SECTION && emu.active_handler(cfg).as_deref() == Some(SHADPS4_HANDLER);
    emu.cards.cemu_menu =
        key == WIIU_SECTION && emu.active_handler(cfg).as_deref() == Some(CEMU_HANDLER);
    let mode = match cover_layout {
        1 => carousel::Mode::VerticalGrid,
        2 => carousel::Mode::HorizontalGrid,
        _ => carousel::Mode::Carousel,
    };
    let action = emu.cards.ui(
        ui,
        rect,
        rect,
        &mut emu.row,
        None,
        false,
        &emu.search.text,
        car_cfg,
        mode,
        alpha,
        interactive,
        false,
        s,
    );
    // Card-order edits persist (Lacrima keeps the section's item
    // order across sessions too).
    if emu.cards.take_reorder_dirty() {
        emu.sync_order(cfg);
    }
    if let Some(note) = &emu.notice {
        ui.painter().text(
            Pos2::new(rect.center().x, rect.max.y - 10.0 * s),
            Align2::CENTER_BOTTOM,
            note,
            FontId::proportional(16.0 * s),
            Color32::from_rgb(0xE0, 0x9A, 0x5A).gamma_multiply(alpha),
        );
    }
    // Covers stream in on resolver threads — a rotating arc plus a
    // one-line label (Lacrima's LoadingControl idiom) at the item
    // area's bottom edge while fetches are still in flight.
    if !emu.meta_rxs.is_empty() {
        ui.ctx().request_repaint();
        let col = Color32::from_rgb(0x2E, 0xB5, 0xFF).gamma_multiply(alpha);
        let r = 9.0 * s;
        let cy = rect.max.y - 34.0 * s;
        let cx = rect.center().x - 58.0 * s;
        let t0 = (ui.ctx().input(|i| i.time) as f32) * std::f32::consts::TAU * 0.7;
        let pts: Vec<Pos2> = (0..=20)
            .map(|i| {
                let a = t0 + i as f32 / 20.0 * std::f32::consts::TAU * 0.78;
                Pos2::new(cx + a.cos() * r, cy + a.sin() * r)
            })
            .collect();
        ui.painter()
            .add(egui::Shape::line(pts, Stroke::new(2.2 * s, col)));
        ui.painter().text(
            Pos2::new(cx + r + 8.0 * s, cy),
            Align2::LEFT_CENTER,
            "Fetching covers…",
            FontId::proportional(13.0 * s),
            col,
        );
    }
    match action {
        // One session at a time. Play on the RUNNING game just brings
        // the capture viewport back; Play on a different game is
        // Lacrima's RequestEmulatorLaunch — queue the request and
        // close the running emulator first, the new launch fires
        // once the old session reports finished.
        carousel::Action::Play(p) => emu.play_card(&p, cfg),
        // "Edit Metadata" — the EmulationMetadataOverlay port.
        carousel::Action::Edit(p) => {
            let platform = emu
                .sections
                .iter()
                .find(|c| section_key(c) == key)
                .map(|c| c.title.clone())
                .unwrap_or_default();
            emu.editor.open(ui.ctx(), p, platform, key == XENIA_SECTION);
        }
        // Xenia per-game overlays — the storage root resolves the
        // same way a launch does (launcher dir ⇄ managed install).
        carousel::Action::Patches(p) => {
            let handler = emu.active_handler(cfg);
            if key == PS3_SECTION && handler.as_deref() == Some(RPCS3_HANDLER) {
                emu.rpcs3_patches
                    .open(&p, &rom_title(&p), rpcs3_storage(cfg));
            } else if key == PS4_SECTION && handler.as_deref() == Some(SHADPS4_HANDLER) {
                emu.shad_patches
                    .open(&p, &rom_title(&p), shadps4_storage(cfg));
            } else {
                emu.patches.open(&p, &rom_title(&p), xenia_storage(cfg));
            }
        }
        // PS4 + shadPS4: the ShadPs4CheatsOverlay (Lacrima's
        // ShadPs4CheatsMenuItem).
        carousel::Action::Cheats(p) => {
            if key == PS4_SECTION && emu.active_handler(cfg).as_deref() == Some(SHADPS4_HANDLER) {
                emu.shad_cheats
                    .open(&p, &rom_title(&p), shadps4_storage(cfg));
            }
        }
        carousel::Action::CustomConfig(p) => {
            emu.cfg_ov.open(&p, &rom_title(&p), xenia_storage(cfg));
        }
        // WIIU + cemu — the CemuGraphicPacksOverlay (Lacrima's
        // CemuGraphicPacksMenuItem).
        carousel::Action::GraphicPacks(p) => {
            if key == WIIU_SECTION && emu.active_handler(cfg).as_deref() == Some(CEMU_HANDLER) {
                emu.cemu_packs.open(&p, &rom_title(&p), cemu_storage(cfg));
            }
        }
        // Confirmed deletes come back through `pending_delete` —
        // main.rs runs them through the shared dialog.
        carousel::Action::Delete(paths) => emu.pending_delete = Some(paths),
        carousel::Action::Clear => emu.clear_games(cfg),
        carousel::Action::ClearCache => emu.clear_section_cache(),
        carousel::Action::AddRoms => {
            let patterns = section_patterns(&key);
            let mut dlg = rfd::FileDialog::new().set_title("Add ROMs");
            if !patterns.is_empty() {
                dlg = dlg.add_filter(&format!("{key} ROMs"), patterns);
            }
            dlg = dlg
                .add_filter("ROM Archives", ARCHIVE_EXTS)
                .add_filter("All Files", &["*"]);
            if let Some(files) = dlg.pick_files() {
                emu.add_games(cfg, files);
            }
        }
        carousel::Action::ScanFolder => {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("Scan folder for ROMs")
                .pick_folder()
            {
                // The recursive walk can take seconds on a big tree —
                // run it off-thread, the results drain in view().
                let key2 = key.clone();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    // A panic mid-walk (a weird filename anywhere in
                    // the tree) must still deliver a result — a dead
                    // sender leaves the UI spinning on "Scanning…".
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        scan_rom_folder(&dir, &key2)
                    }))
                    .unwrap_or_default();
                    let _ = tx.send(res);
                });
                emu.scan_rx = Some(rx);
                emu.notice = Some("Scanning folder…".into());
            }
        }
        _ => {}
    }
}

/// ScanFolder — Lacrima's `ScanFolderForRomPaths`: depth-first walk
/// for the section's handler patterns (plus the archive extensions
/// Lacrima always adds), dot-files skipped. For PS3/PS4 an installed-
/// game folder (`Ps3InstalledGameHelper.IsInstalledGameFolder`) is
/// added whole as ONE rom and never descended into — its inner
/// .pkg/.bin files are never scanned. Runs on a worker; the caller
/// drains the result off the UI thread.
fn scan_rom_folder(root: &Path, key: &str) -> Vec<PathBuf> {
    let pats = section_patterns(key);
    // EmulationConsoleCatalog.SupportsFolderImport — PS3/PS4 each get
    // their own installed-folder check.
    let accept = |ext: &str| {
        pats.iter().any(|p| p.eq_ignore_ascii_case(ext)) || ARCHIVE_EXTS.contains(&ext)
    };
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    // Canonical dedup — a directory junction pointing back at an
    // ancestor would loop the DFS forever.
    let mut visited = HashSet::new();
    while let Some(dir) = stack.pop() {
        let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !visited.insert(canon) {
            continue;
        }
        let installed = match key {
            "PS3" => emu_meta::ps3_is_installed_folder(&dir),
            "PS4" => emu_meta::ps4_is_installed_folder(&dir),
            _ => false,
        };
        if installed {
            out.push(dir);
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            // ShouldSkipFilesystemEntry — dot/hidden + macOS `._`.
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| accept(&x.to_lowercase()))
            {
                out.push(p);
            }
        }
    }
    // CollapseDiscImageArtifacts — a cue/gdi/m3u descriptor owns its
    // track files; they aren't separate roms.
    let desc = |p: &Path| {
        matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .as_deref(),
            Some("cue" | "gdi" | "m3u")
        )
    };
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let referenced: std::collections::HashSet<String> = out
        .iter()
        .filter(|p| desc(p))
        .flat_map(|p| emu_meta::descriptor_refs(p))
        .map(|r| norm(&r))
        .collect();
    if !referenced.is_empty() {
        out.retain(|p| desc(p) || !referenced.contains(&norm(p)));
    }
    out.sort();
    out
}

/// The Settings-overlay Emulation tab (Lacrima's SettingsOverlay
/// "Emulation" TabItem): Miscellaneous toggles, the Gameplay
/// Recording output folder, and the expandable Consoles cards.
/// Handler controls render as placeholders — wiring lands with the
/// emulator-session work.
pub fn settings_tab(
    ui: &mut egui::Ui,
    panel: Rect,
    t: f32,
    emu: &mut EmuView,
    cfg: &mut settings::Emulation,
    s: f32,
) {
    // Same scrollable-body pattern as the Audio tab: paint into the
    // panel, clip to the band between the tab strip and the footer,
    // track the offset in persisted data.
    // Clip ends just above the footer separator (-70s) — content must
    // never underlap the "Open AES Directory" row.
    let body = Rect::from_min_max(
        Pos2::new(panel.min.x, panel.min.y + 78.0 * s),
        Pos2::new(panel.max.x, panel.max.y - 74.0 * s),
    );
    let scroll_id = ui.id().with("emu-scroll");
    let mut scroll = ui
        .ctx()
        .data_mut(|d| d.get_persisted::<f32>(scroll_id).unwrap_or(0.0));
    if ui
        .ctx()
        .input(|i| i.pointer.hover_pos().is_some_and(|p| body.contains(p)))
    {
        let dy = ui.ctx().input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            scroll -= dy;
        }
    }
    scroll = scroll.max(0.0);

    let mut cui = ui.new_child(UiBuilder::new().max_rect(panel));
    cui.set_clip_rect(body);
    let painter = cui.painter().clone();
    let x = panel.min.x + 30.0 * s;
    let right = panel.max.x - 30.0 * s;
    let mut y = panel.min.y + 95.0 * s - scroll;
    let whitesmoke = Color32::from_rgb(0xF5, 0xF5, 0xF5);
    let darkgray = Color32::from_gray(0xA9);

    let header = |painter: &egui::Painter, y: &mut f32, name: &str| {
        painter.text(
            Pos2::new(x, *y),
            Align2::LEFT_TOP,
            name,
            FontId::proportional(30.0 * s),
            Color32::WHITE.gamma_multiply(t),
        );
        *y += 46.0 * s;
        painter.line_segment(
            [Pos2::new(x, *y), Pos2::new(right, *y)],
            Stroke::new(1.0, Color32::from_gray(55).gamma_multiply(t)),
        );
        *y += 25.0 * s;
    };
    let label = FontId::proportional(20.0 * s);
    let toggle_row = |ui: &mut egui::Ui, y: f32, name: &str, on: &mut bool, salt: &str| {
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_TOP,
            name,
            label.clone(),
            whitesmoke.gamma_multiply(t),
        );
        let w = painter
            .layout_no_wrap(name.to_owned(), label.clone(), Color32::WHITE)
            .rect
            .width();
        let sw = Rect::from_min_size(
            Pos2::new(x + w + 8.0 * s, y - 2.0 * s),
            Vec2::new(44.0 * s, 22.0 * s),
        );
        crate::toggle_switch(ui, sw, salt, on, t);
    };

    // --- Miscellaneous ------------------------------------------------
    header(&painter, &mut y, "Miscellaneous");
    toggle_row(
        &mut cui,
        y,
        "Use first ROM item cover for album tiles",
        &mut cfg.use_first_item_cover,
        "emu-first-cover",
    );
    y += 40.0 * s;
    toggle_row(
        &mut cui,
        y,
        "Gameplay Autoplay",
        &mut cfg.gameplay_autoplay,
        "emu-autoplay",
    );
    y += 40.0 * s;
    toggle_row(
        &mut cui,
        y,
        "Fill letterbox bars with back cover art",
        &mut cfg.back_cover_letterbox_fill,
        "emu-letterbox",
    );
    y += 55.0 * s;

    // --- Gameplay Recording -------------------------------------------
    header(&painter, &mut y, "Gameplay Recording");
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Output folder",
        label.clone(),
        whitesmoke.gamma_multiply(t),
    );
    y += 34.0 * s;
    // TextBox (#2B2B2B fill, DimGray border) + Browse button, Lacrima's
    // "*,Auto" column pair with 10px spacing.
    let btn_w = 90.0 * s;
    let box_rect = Rect::from_min_size(
        Pos2::new(x, y),
        Vec2::new((right - x - btn_w - 10.0 * s).max(60.0 * s), 30.0 * s),
    );
    let mut tui = cui.new_child(
        UiBuilder::new()
            .max_rect(box_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    tui.set_opacity(t);
    tui.add(
        egui::TextEdit::singleline(&mut cfg.recording_dir)
            .desired_width(box_rect.width())
            .hint_text("Folder for recorded videos")
            .font(FontId::proportional(16.0 * s)),
    );
    painter.rect_filled(
        box_rect,
        CornerRadius::same(4),
        Color32::from_rgb(0x2B, 0x2B, 0x2B).gamma_multiply(0.4 * t),
    );
    painter.rect_stroke(
        box_rect,
        CornerRadius::same(4),
        Stroke::new(1.0, Color32::from_gray(105).gamma_multiply(t)),
        StrokeKind::Outside,
    );
    let (clicked, _) = crate::flat_button(
        &mut cui,
        Pos2::new(box_rect.max.x + 10.0 * s, y - 1.0 * s),
        "Browse",
        s,
        true,
        Color32::from_rgb(0x22, 0x22, 0x22),
        Color32::WHITE,
        t,
    );
    if clicked {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Folder for recorded videos")
            .pick_folder()
        {
            cfg.recording_dir = dir.to_string_lossy().into_owned();
        }
    }
    y += 62.0 * s;

    // --- Consoles -----------------------------------------------------
    header(&painter, &mut y, "Consoles");
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Expand a console section to configure the supported emulator handlers for it.",
        FontId::proportional(16.0 * s),
        darkgray.gamma_multiply(t),
    );
    y += 40.0 * s;

    // Section cards: #151515 fill, #343434 border, radius 8, 12px gap
    // — Lacrima's EmulationSectionItem template. The cards mirror the
    // Emulation view's strip order — both render `emu.sections`, so a
    // persisted or just-dragged order shows identically here.
    let ordered: Vec<(String, String, String)> = emu
        .sections
        .iter()
        .map(|s| {
            let k = section_key(s);
            let art = CONSOLES
                .iter()
                .find(|(ck, _, _)| *ck == k)
                .map(|(_, _, a)| (*a).to_string())
                .unwrap_or_default();
            (k, s.title.clone(), art)
        })
        .collect();
    for (key, title, art) in &ordered {
        let key = key.as_str();
        let open = emu.expanded.contains(key);
        let head_h = 88.0 * s;
        // Sections with a wired handler host the handler row — a bit
        // taller than the empty-state cards. RetroArch cards get one
        // more row (the core selector pushes the buttons + status
        // slot down ~32px), so their body grows to match.
        let body_h = if open {
            match handler_spec(key) {
                Some(spec)
                    if spec.handler == RETROARCH_HANDLER
                        && libretro::Section::from_key(key).is_some() =>
                {
                    228.0 * s
                }
                Some(_) => 196.0 * s,
                None => 74.0 * s,
            }
        } else {
            0.0
        };
        let card = Rect::from_min_size(Pos2::new(x, y), Vec2::new(right - x, head_h + body_h));
        painter.rect_filled(
            card,
            CornerRadius::same((8.0 * s) as u8),
            Color32::from_rgb(0x15, 0x15, 0x15).gamma_multiply(t),
        );
        painter.rect_stroke(
            card,
            CornerRadius::same((8.0 * s) as u8),
            Stroke::new(1.0, Color32::from_rgb(0x34, 0x34, 0x34).gamma_multiply(t)),
            StrokeKind::Inside,
        );
        let head = Rect::from_min_size(card.min, Vec2::new(card.width(), head_h));
        let resp = cui.interact(head, cui.id().with(("emu-section", key)), Sense::click());
        if resp.clicked() {
            if open {
                emu.expanded.remove(key);
            } else {
                emu.expanded.insert(key.to_string());
            }
        }
        if resp.hovered() {
            cui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // 96x60 console-art thumb (border + corner clip), title, then
        // the +/- expander glyph on the right.
        let thumb = Rect::from_min_size(
            card.min + Vec2::new(14.0 * s, 14.0 * s),
            Vec2::new(96.0 * s, 60.0 * s),
        );
        painter.rect_filled(
            thumb,
            CornerRadius::same((8.0 * s) as u8),
            Color32::BLACK.gamma_multiply(t),
        );
        if !art.is_empty() {
            let src = library::CoverSrc::File(
                settings::data_root()
                    .map(|d| d.join("Assets").join("Consoles").join(format!("{art}.png")))
                    .unwrap_or_default(),
            );
            if let Some(tex) = emu.row.cover_tex(ui.ctx(), &src) {
                paint::paint_cover(
                    &painter,
                    tex.id(),
                    tex.size(),
                    thumb.shrink(1.0),
                    8.0 * s,
                    t,
                );
            }
        }
        painter.rect_stroke(
            thumb,
            CornerRadius::same((8.0 * s) as u8),
            Stroke::new(1.0, Color32::from_rgb(0x34, 0x34, 0x34).gamma_multiply(t)),
            StrokeKind::Inside,
        );
        painter.text(
            Pos2::new(thumb.max.x + 14.0 * s, head.center().y),
            Align2::LEFT_CENTER,
            title,
            label.clone(),
            whitesmoke.gamma_multiply(t),
        );
        painter.text(
            Pos2::new(card.max.x - 14.0 * s - 14.0 * s, head.center().y),
            Align2::CENTER_CENTER,
            if open { "-" } else { "+" },
            FontId::proportional(24.0 * s),
            darkgray.gamma_multiply(t),
        );
        if open {
            let inner = Rect::from_min_size(
                Pos2::new(card.min.x + 14.0 * s, card.min.y + head_h),
                Vec2::new(card.width() - 28.0 * s, body_h - 14.0 * s),
            );
            painter.rect_filled(
                inner,
                CornerRadius::same((8.0 * s) as u8),
                Color32::from_rgb(0x1D, 0x1D, 0x1D).gamma_multiply(t),
            );
            painter.rect_stroke(
                inner,
                CornerRadius::same((8.0 * s) as u8),
                Stroke::new(1.0, Color32::from_rgb(0x34, 0x34, 0x34).gamma_multiply(t)),
                StrokeKind::Inside,
            );
            if let Some(spec) = handler_spec(key) {
                // Handler row — Lacrima's EmulationHandlerAppItem:
                // name, launcher path, then the right-aligned
                // Set-default/Browse/Clear/Download row, with the
                // download progress block underneath.
                let m = 12.0 * s;
                let ry = inner.min.y + m;
                // RetroArch-backed sections (3DS, N64) get a second
                // row: the core selector beneath the retroarch.exe
                // path box — listing only THAT section's cores.
                let core_section = libretro::Section::from_key(key);
                let core_select = spec.handler == RETROARCH_HANDLER && core_section.is_some();
                let font14 = FontId::proportional(14.0 * s);
                let name_font = FontId::proportional(18.0 * s);
                painter.text(
                    Pos2::new(inner.min.x + m, ry + 2.0 * s),
                    Align2::LEFT_TOP,
                    spec.name,
                    name_font.clone(),
                    whitesmoke.gamma_multiply(t),
                );
                // Installed/version hint next to the name — offset by
                // the name's real width (a fixed 64px underlaps
                // "RetroArch" at 18pt) and clipped to the card so a
                // long path can't run off the right edge.
                {
                    let stale = emu.installed_hint.get(spec.handler).map_or(true, |(t, _)| {
                        t.elapsed() > std::time::Duration::from_millis(1500)
                    });
                    if stale {
                        let exe = (spec.installed_exe)();
                        emu.installed_hint
                            .insert(spec.handler.to_string(), (std::time::Instant::now(), exe));
                    }
                    if let Some(Some(exe)) = emu.installed_hint.get(spec.handler).map(|(_, p)| p) {
                        let name_w = painter
                            .layout_no_wrap(spec.name.to_owned(), name_font.clone(), whitesmoke)
                            .rect
                            .width();
                        let hint_x = inner.min.x + m + name_w + 10.0 * s;
                        let hint_clip = Rect::from_min_max(
                            Pos2::new(hint_x, inner.min.y),
                            Pos2::new(inner.max.x - m, ry + 30.0 * s),
                        );
                        painter.with_clip_rect(hint_clip).text(
                            Pos2::new(hint_x, ry + 7.0 * s),
                            Align2::LEFT_TOP,
                            exe.parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                            FontId::proportional(12.0 * s),
                            darkgray.gamma_multiply(t * 0.8),
                        );
                    }
                }

                // Path TextBox — Lacrima shows the launcher path; ours
                // is directly editable too.
                let box_rect = Rect::from_min_size(
                    Pos2::new(inner.min.x + m, ry + 32.0 * s),
                    Vec2::new(inner.width() - m * 2.0, 26.0 * s),
                );
                let mut tui = cui.new_child(
                    UiBuilder::new()
                        .max_rect(box_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                tui.set_opacity(t);
                let entry = cfg.emulators.entry(spec.handler.to_string()).or_default();
                tui.add(
                    egui::TextEdit::singleline(entry)
                        .desired_width(box_rect.width())
                        .hint_text(egui::RichText::new(spec.exe_hint).font(font14.clone()))
                        .font(font14.clone()),
                );
                let mut combo_response = None;
                let mut combo_open = false;
                let mut combo_text = String::new();
                let mut combo_rect = box_rect;
                if let Some(core_section) = core_section.filter(|_| core_select) {
                    combo_rect = Rect::from_min_size(
                        Pos2::new(inner.min.x + m, ry + 64.0 * s),
                        Vec2::new(inner.width() - m * 2.0, 26.0 * s),
                    );
                    let mut cui2 = cui.new_child(
                        UiBuilder::new()
                            .max_rect(combo_rect)
                            .layout(Layout::left_to_right(Align::Center)),
                    );
                    cui2.set_opacity(t);
                    if emu.cores_hint.get(key).map_or(true, |(t, _)| {
                        t.elapsed() > std::time::Duration::from_secs(1)
                    }) {
                        emu.cores_hint.insert(
                            key.to_string(),
                            (
                                std::time::Instant::now(),
                                libretro::installed_cores(core_section),
                            ),
                        );
                    }
                    let cores = emu
                        .cores_hint
                        .get(key)
                        .map(|(_, c)| c.clone())
                        .unwrap_or_default();
                    if let Some(first) = cores.first() {
                        let sel = selected_core(cfg, core_section);
                        if !cores.iter().any(|c| c.id.eq_ignore_ascii_case(sel)) {
                            *sel = first.id.clone();
                            emu.settings_dirty = true;
                        }
                    }
                    let selected = cores
                        .iter()
                        .find(|c| {
                            c.id.eq_ignore_ascii_case(selected_core(cfg, core_section).as_str())
                        })
                        .or_else(|| cores.first());
                    combo_text = selected
                        .map(|c| c.display_name.clone())
                        .unwrap_or_else(|| "No cores installed".to_string());
                    let response = cui2.interact(
                        combo_rect,
                        cui2.id().with(format!("{key}-libretro-core")),
                        Sense::click(),
                    );
                    let popup_id = egui::Popup::default_response_id(&response);
                    combo_open = egui::Popup::is_id_open(cui2.ctx(), popup_id);
                    if !cores.is_empty() {
                        egui::Popup::menu(&response)
                            .width(combo_rect.width())
                            .show(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 0.0;
                                egui::ScrollArea::vertical()
                                    .max_height(180.0 * s)
                                    .show(ui, |ui| {
                                        ui.style_mut().wrap_mode =
                                            Some(egui::TextWrapMode::Truncate);
                                        for core in &cores {
                                            if ui
                                                .selectable_label(
                                                    selected_core(cfg, core_section)
                                                        .eq_ignore_ascii_case(&core.id),
                                                    RichText::new(&core.display_name)
                                                        .font(font14.clone()),
                                                )
                                                .clicked()
                                                && *selected_core(cfg, core_section) != core.id
                                            {
                                                *selected_core(cfg, core_section) = core.id.clone();
                                                emu.settings_dirty = true;
                                            }
                                        }
                                    });
                            });
                    }
                    combo_response = Some(response);
                }
                painter.rect_filled(
                    box_rect,
                    CornerRadius::same(4),
                    Color32::from_rgb(0x2B, 0x2B, 0x2B).gamma_multiply(0.4 * t),
                );
                painter.rect_stroke(
                    box_rect,
                    CornerRadius::same(4),
                    Stroke::new(1.0, Color32::from_gray(105).gamma_multiply(t)),
                    StrokeKind::Outside,
                );
                if let Some(response) = combo_response {
                    let fill = if response.is_pointer_button_down_on() {
                        Color32::from_rgb(0x20, 0x20, 0x20)
                    } else if response.hovered() || combo_open {
                        Color32::from_rgb(0x34, 0x34, 0x34)
                    } else {
                        Color32::from_rgb(0x2B, 0x2B, 0x2B)
                    };
                    painter.rect_filled(
                        combo_rect,
                        CornerRadius::same(4),
                        fill.gamma_multiply(0.55 * t),
                    );
                    painter.rect_stroke(
                        combo_rect,
                        CornerRadius::same(4),
                        Stroke::new(
                            1.0,
                            Color32::from_gray(if response.hovered() { 125 } else { 85 })
                                .gamma_multiply(t),
                        ),
                        StrokeKind::Inside,
                    );
                    let text_clip = Rect::from_min_max(
                        Pos2::new(combo_rect.min.x + 10.0 * s, combo_rect.min.y),
                        Pos2::new(combo_rect.max.x - 30.0 * s, combo_rect.max.y),
                    );
                    let text_color = if combo_text == "No cores installed" {
                        darkgray
                    } else {
                        whitesmoke
                    };
                    painter.with_clip_rect(text_clip).text(
                        Pos2::new(text_clip.min.x, combo_rect.center().y),
                        Align2::LEFT_CENTER,
                        combo_text,
                        font14.clone(),
                        text_color.gamma_multiply(t),
                    );
                    let chevron = Rect::from_center_size(
                        Pos2::new(combo_rect.max.x - 17.0 * s, combo_rect.center().y),
                        Vec2::new(9.0 * s, 5.0 * s),
                    );
                    let points = if combo_open {
                        vec![
                            chevron.left_bottom(),
                            chevron.right_bottom(),
                            chevron.center_top(),
                        ]
                    } else {
                        vec![
                            chevron.left_top(),
                            chevron.right_top(),
                            chevron.center_bottom(),
                        ]
                    };
                    painter.add(Shape::convex_polygon(
                        points,
                        Color32::from_gray(145).gamma_multiply(t),
                        Stroke::NONE,
                    ));
                    if response.hovered() {
                        cui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                }

                // Right-aligned button row — Lacrima's
                // Set default | Browse | Clear | Download. A job
                // under a sibling section's key counts too — shared
                // handlers (RetroArch, Dolphin) install into one
                // directory, so only one can run at a time.
                let downloading = emu.dl.values().any(|j| j.handler == spec.handler);
                let is_default = cfg
                    .default_handlers
                    .get(spec.section)
                    .is_some_and(|h| h == spec.handler);
                let has_path = cfg
                    .emulators
                    .get(spec.handler)
                    .is_some_and(|p| !p.trim().is_empty());
                let btn_font = FontId::proportional(14.0 * s);
                let measure = |p: &egui::Painter, text: &str| {
                    p.layout_no_wrap(text.to_owned(), btn_font.clone(), Color32::WHITE)
                        .rect
                        .width()
                        + 30.0 * s
                };
                // The extra combo row pushes the buttons down on the
                // 3DS card.
                let by = ry + if core_select { 96.0 } else { 68.0 } * s;
                let mut bx = inner.max.x - m;

                // Download (green #1E6B3A) — reads "Update" once a
                // managed install exists; the worker path is the same
                // (fetch newest release → overwrite the managed dir).
                let dl_label =
                    if emu.installed_hint.get(spec.handler).is_some_and(|(_, p)| p.is_some()) {
                        "Update"
                    } else {
                        "Download"
                    };
                let w = measure(&painter, dl_label);
                bx -= w;
                let (dl_clicked, _) = crate::flat_button(
                    &mut cui,
                    Pos2::new(bx, by),
                    dl_label,
                    s,
                    !downloading,
                    Color32::from_rgb(0x1E, 0x6B, 0x3A),
                    Color32::WHITE,
                    t,
                );
                if dl_clicked {
                    emu.dl.insert(
                        key.to_string(),
                        DlJob {
                            handler: spec.handler,
                            name: spec.name,
                            cores: None,
                            shared: (spec.download)(),
                        },
                    );
                    emu.dl_msg.remove(key);
                }
                bx -= 12.0 * s;

                // RetroArch-backed sections: the Libretro core
                // packages download separately from the frontend —
                // and only the cores matching THIS section's filter.
                if let Some(core_section) = core_section.filter(|_| core_select) {
                    let w = measure(&painter, "Download all cores");
                    bx -= w;
                    let (cores_clicked, _) = crate::flat_button(
                        &mut cui,
                        Pos2::new(bx, by),
                        "Download all cores",
                        s,
                        !downloading,
                        Color32::from_rgb(0x1E, 0x6B, 0x3A),
                        Color32::WHITE,
                        t,
                    );
                    if cores_clicked {
                        emu.dl.insert(
                            key.to_string(),
                            DlJob {
                                handler: spec.handler,
                                name: spec.name,
                                cores: Some(core_section),
                                shared: libretro::download_cores(core_section),
                            },
                        );
                        emu.dl_msg.remove(key);
                    }
                    bx -= 12.0 * s;
                }

                // Clear (dark red #3A2020 — Lacrima shows it only when
                // a path is set).
                if has_path {
                    let w = measure(&painter, "Clear");
                    bx -= w;
                    let (clr, _) = crate::flat_button(
                        &mut cui,
                        Pos2::new(bx, by),
                        "Clear",
                        s,
                        true,
                        Color32::from_rgb(0x3A, 0x20, 0x20),
                        Color32::WHITE,
                        t,
                    );
                    if clr {
                        cfg.emulators.remove(spec.handler);
                        emu.settings_dirty = true;
                    }
                    bx -= 12.0 * s;
                }

                // Browse.
                {
                    let w = measure(&painter, "Browse");
                    bx -= w;
                    let (browse, _) = crate::flat_button(
                        &mut cui,
                        Pos2::new(bx, by),
                        "Browse",
                        s,
                        true,
                        Color32::from_rgb(0x22, 0x22, 0x22),
                        Color32::WHITE,
                        t,
                    );
                    if browse {
                        if let Some(file) = rfd::FileDialog::new()
                            .set_title(spec.dialog_title)
                            .add_filter(spec.name, &["exe"])
                            .pick_file()
                        {
                            cfg.emulators
                                .insert(spec.handler.into(), file.to_string_lossy().into_owned());
                            emu.settings_dirty = true;
                        }
                    }
                    bx -= 12.0 * s;
                }

                // Set default (blue #1E4B7A) / Default chip (#7CFF7C).
                if is_default {
                    painter.text(
                        Pos2::new(bx - 8.0 * s, by + 15.0 * s),
                        Align2::RIGHT_CENTER,
                        "Default",
                        btn_font.clone(),
                        Color32::from_rgb(0x7C, 0xFF, 0x7C).gamma_multiply(t),
                    );
                } else {
                    let w = measure(&painter, "Set default");
                    bx -= w;
                    let (set_def, _) = crate::flat_button(
                        &mut cui,
                        Pos2::new(bx, by),
                        "Set default",
                        s,
                        true,
                        Color32::from_rgb(0x1E, 0x4B, 0x7A),
                        Color32::WHITE,
                        t,
                    );
                    if set_def {
                        cfg.default_handlers
                            .insert(spec.section.into(), spec.handler.into());
                        emu.settings_dirty = true;
                    }
                }

                // Progress block (Lacrima's IsDownloading row) + the
                // last result line. Finished jobs are released by
                // whichever card renders first — the result is keyed
                // to the owner so the right card shows the message
                // and refreshes the right core list.
                let py = by + 38.0 * s;
                let finished: Vec<String> = emu
                    .dl
                    .iter()
                    .filter(|(_, job)| {
                        job.shared
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .done
                            .is_some()
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in finished {
                    if let Some(job) = emu.dl.remove(&k) {
                        let res = job
                            .shared
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .done
                            .clone()
                            .unwrap_or_else(|| Err("download aborted".to_string()));
                        let msg = match res {
                            Ok(result) => {
                                if let Some(core_section) = job.cores {
                                    // The core pack returns a summary,
                                    // not a path — re-default the
                                    // selection to the first install
                                    // when the stored id is gone.
                                    let cores = libretro::installed_cores(core_section);
                                    emu.cores_hint.insert(
                                        core_section.key().to_string(),
                                        (std::time::Instant::now(), cores.clone()),
                                    );
                                    if let Some(first) = cores.first() {
                                        let sel = selected_core(cfg, core_section);
                                        if !cores.iter().any(|c| c.id.eq_ignore_ascii_case(sel)) {
                                            *sel = first.id.clone();
                                        }
                                    }
                                    emu.settings_dirty = true;
                                    result
                                } else {
                                    cfg.emulators.insert(job.handler.into(), result);
                                    emu.settings_dirty = true;
                                    emu.installed_hint.remove(job.handler);
                                    format!("{} installed", job.name)
                                }
                            }
                            Err(e) => e,
                        };
                        emu.dl_msg.insert(k, msg);
                    }
                }
                let mut status_line = emu.dl_msg.get(key).cloned();
                if let Some(job) = emu.dl.get(key) {
                    let (stage, frac) = {
                        let p = job.shared.lock().unwrap_or_else(|e| e.into_inner());
                        (p.stage.clone(), p.frac)
                    };
                    // Keep repainting while the download runs —
                    // egui is reactive, so without this the bar
                    // only moves on input events.
                    ui.ctx().request_repaint();
                    let track = Rect::from_min_size(
                        Pos2::new(inner.min.x + m, py),
                        Vec2::new(inner.width() - m * 2.0, 6.0 * s),
                    );
                    painter.rect_filled(
                        track,
                        CornerRadius::same(3),
                        Color32::from_gray(50).gamma_multiply(t),
                    );
                    painter.rect_filled(
                        Rect::from_min_size(
                            track.min,
                            Vec2::new(track.width() * frac, track.height()),
                        ),
                        CornerRadius::same(3),
                        Color32::from_rgb(0x1E, 0x6B, 0x3A).gamma_multiply(t),
                    );
                    status_line = Some(stage);
                }
                // Status + hint share the card's inner width — clip
                // both so a long download summary can't spill past
                // the right edge and over the next card.
                let foot_clip = Rect::from_min_max(
                    Pos2::new(inner.min.x + m, inner.min.y),
                    Pos2::new(inner.max.x - m, inner.max.y),
                );
                let foot_painter = painter.with_clip_rect(foot_clip);
                if let Some(msg) = status_line {
                    // Fixed slot above the hint — ~14px of clear
                    // margin between the status and the hint text.
                    foot_painter.text(
                        Pos2::new(inner.min.x + m, inner.max.y - 36.0 * s),
                        Align2::LEFT_BOTTOM,
                        msg,
                        FontId::proportional(13.0 * s),
                        Color32::from_gray(0xCC).gamma_multiply(t),
                    );
                }
                // Launch hint — where games come from.
                foot_painter.text(
                    Pos2::new(inner.min.x + 12.0 * s, inner.max.y - 10.0 * s),
                    Align2::LEFT_BOTTOM,
                    "Add games in the Emulation view (right-click → Add Roms), then double-click a card to play.",
                    FontId::proportional(13.0 * s),
                    darkgray.gamma_multiply(t),
                );
            } else {
                painter.text(
                    inner.center(),
                    Align2::CENTER_CENTER,
                    "No emulator handlers configured for this section yet",
                    FontId::proportional(16.0 * s),
                    darkgray.gamma_multiply(t),
                );
            }
        }
        y += card.height() + 12.0 * s;
    }

    // Scroll bookkeeping: clamp against the real content height and
    // show the thin scrollbar thumb when it overflows.
    let content_h = (y + scroll) - (panel.min.y + 95.0 * s);
    let max_scroll = (content_h - body.height()).max(0.0);
    scroll = scroll.min(max_scroll);
    ui.ctx().data_mut(|d| d.insert_persisted(scroll_id, scroll));
    if max_scroll > 1.0 {
        let thumb_h = (body.height() * (body.height() / content_h)).max(24.0);
        let thumb_y = body.min.y + (body.height() - thumb_h) * (scroll / max_scroll);
        let thumb = Rect::from_min_size(
            Pos2::new(body.max.x - 5.0, thumb_y),
            Vec2::new(3.0, thumb_h),
        );
        ui.painter().rect_filled(
            thumb,
            CornerRadius::same(2),
            Color32::from_white_alpha(70).gamma_multiply(t),
        );
    }
}

/// Lacrima's RenderOptionsOverlay — a right-side panel covering the
/// item area only (the band and section strip stay visible). Only
/// the first ("Rendering") tab is implemented; every control writes
/// the loaded section's RenderOpts, which view() writes through to
/// `cfg.render[section]` per change — Lacrima's SectionSettings
/// AutoSave.
fn render_panel(
    ui: &mut egui::Ui,
    emu: &mut EmuView,
    area: Rect,
    alpha: f32,
    interactive: bool,
    s: f32,
) {
    // The panel only exists while a capture runs — a session ending
    // slides it closed instead of leaving a floating settings slab.
    if emu.session.is_none() {
        emu.render_open = false;
    }
    // Same slide as the global settings overlay — 0.3s wall-clock
    // animate_bool + ease_out_cubic; the panel just slides out and
    // back over the content.
    let slide =
        ui.ctx()
            .animate_bool_with_time(ui.id().with("render-panel-slide"), emu.render_open, 0.3);
    let t = crate::ease_out_cubic(slide);
    if t <= 0.001 {
        return;
    }
    // The view scale `s` shrinks with the window — panel contents
    // stay readable via a floor; geometry matches the settings
    // overlay (38% slab over the full content height).
    let ps = s.clamp(0.56, 0.82);
    let w = (area.width() * 0.30).clamp(230.0, area.width());
    let panel = Rect::from_min_max(Pos2::new(area.max.x - w * t, area.min.y), area.max);
    let painter = ui.painter().clone();
    painter.rect_filled(panel, 0.0, Color32::from_black_alpha((242.0 * alpha) as u8));
    painter.vline(
        panel.min.x,
        panel.y_range(),
        Stroke::new(s.max(1.0), Color32::from_white_alpha((51.0 * alpha) as u8)),
    );
    // Clicking the exposed content left of the panel closes it —
    // the panel itself swallows clicks (settings overlay pattern).
    let outside = Rect::from_min_max(area.min, Pos2::new(panel.min.x, area.max.y));
    if ui
        .interact(outside, ui.id().with("render-scrim"), Sense::click())
        .clicked()
    {
        emu.render_open = false;
    }
    let _ = ui.interact(panel, ui.id().with("render-panel"), Sense::click());

    // Contents live in a FIXED-WIDTH rect anchored at panel.min
    // and extending past the screen edge (settings-overlay style:
    // contents are laid out once at full width and translate with
    // the panel). If `inner` were derived from the visible `panel`
    // sliver, its width would grow during the slide and egui would
    // reflow the rows every frame — the "area shrinking" look.
    let inner = Rect::from_min_max(
        panel.min + Vec2::new(28.0 * ps, 18.0 * ps),
        Pos2::new(panel.min.x + w - 28.0 * ps, area.max.y - 18.0 * ps),
    );
    let mut pui = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    pui.set_clip_rect(inner.intersect(area));
    if !(interactive && slide > 0.999) {
        pui.disable();
    }

    let before = emu.render.clone();
    let accent = Color32::from_rgb(0x12, 0xC0, 0xF0);

    // Header — the tab name at title size + a painted X close
    // (the "✕" char isn't in the UI font — it rendered as a
    // checkbox-like tofu box).
    let mut close = false;
    pui.horizontal(|ui| {
        let r = ui.label(
            RichText::new("Rendering")
                .size(20.0 * ps)
                .strong()
                .color(Color32::WHITE),
        );
        // Accent underline marks it as the selected tab — future
        // tabs render at the same title size.
        ui.painter().hline(
            r.rect.min.x..=r.rect.max.x,
            r.rect.max.y + 3.0 * ps,
            Stroke::new(2.0 * ps, accent),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (xr, resp) = ui.allocate_exact_size(Vec2::splat(26.0 * ps), Sense::click());
            if resp.clicked() {
                close = true;
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            let c = xr.center();
            let k = 5.5 * ps;
            let stroke = Stroke::new(
                1.8 * ps,
                (if resp.hovered() {
                    Color32::WHITE
                } else {
                    Color32::from_gray(170)
                })
                .gamma_multiply(alpha),
            );
            ui.painter()
                .line_segment([c + Vec2::new(-k, -k), c + Vec2::new(k, k)], stroke);
            ui.painter()
                .line_segment([c + Vec2::new(k, -k), c + Vec2::new(-k, k)], stroke);
        });
    });
    if close {
        emu.render_open = false;
    }
    pui.add_space(6.0 * ps);
    ui.painter().hline(
        inner.min.x..=inner.max.x,
        pui.cursor().min.y,
        Stroke::new(1.0, Color32::from_white_alpha(28)),
    );
    pui.add_space(14.0 * ps);

    // Rows — scrollable if the item area is shorter than the list.
    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(&mut pui, |pui| {
            render_panel_rows(pui, emu, ps);
        });

    if emu.render != before {
        emu.render_dirty = true;
    }
}

fn render_panel_rows(pui: &mut egui::Ui, emu: &mut EmuView, ps: f32) {
    // SelectedStretch — Uniform / UniformToFill / Fill.
    pui.horizontal(|ui| {
        row_label(ui, "Stretch", ps);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            egui::ComboBox::from_id_salt("render-stretch")
                .selected_text(
                    ["Uniform", "Uniform to Fill", "Fill"][emu.render.stretch.min(2) as usize],
                )
                .width(170.0 * ps)
                .show_ui(ui, |ui| {
                    for (i, n) in ["Uniform", "Uniform to Fill", "Fill"].iter().enumerate() {
                        ui.selectable_value(&mut emu.render.stretch, i as u8, *n);
                    }
                });
        });
    });
    pui.add_space(10.0 * ps);

    render_slider(
        pui,
        "Brightness",
        &mut emu.render.brightness,
        0.25,
        2.0,
        1.0,
        ps,
    );
    render_slider(
        pui,
        "Saturation",
        &mut emu.render.saturation,
        0.0,
        2.0,
        1.0,
        ps,
    );
    render_slider(
        pui,
        "Overlay Opacity",
        &mut emu.render.overlay_opacity,
        0.05,
        1.0,
        0.55,
        ps,
    );

    render_toggle(pui, "Disable VSync", &mut emu.render.disable_vsync, ps);
    render_toggle(
        pui,
        "Remove side pillars",
        &mut emu.render.remove_pillars,
        ps,
    );
    pui.add_space(6.0 * ps);
    render_toggle(pui, "Show Stats Overlay", &mut emu.render.stats_overlay, ps);
    render_toggle(
        pui,
        "Show Frametime Graph",
        &mut emu.render.frametime_graph,
        ps,
    );
    render_toggle(pui, "Show GPU Info", &mut emu.render.gpu_info, ps);
    pui.add_space(6.0 * ps);

    // PortalCaptureTint — an RGB multiply over the capture.
    pui.horizontal(|ui| {
        row_label(ui, "Color Tint", ps);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(egui::Button::new(RichText::new("Reset").size(12.0 * ps)).frame(true))
                .on_hover_text("Reset to white (no tint)")
                .clicked()
            {
                emu.render.tint = [255, 255, 255];
            }
            let [r, g, b] = emu.render.tint;
            let mut c = Color32::from_rgb(r, g, b);
            if ui.color_edit_button_srgba(&mut c).changed() {
                emu.render.tint = [c.r(), c.g(), c.b()];
            }
        });
    });
    pui.add_space(6.0 * ps);

    // SelectedShaderPath — the bundled capture post-process shaders
    // (Lacrima's "Shader file" ComboBox); hidden when the GPU path
    // isn't available.
    if emu.cap_shader.is_some() {
        pui.horizontal(|ui| {
            row_label(ui, "Shader", ps);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let selected = if emu.render.shader.is_empty() {
                    "None".to_string()
                } else {
                    emu.render.shader.clone()
                };
                egui::ComboBox::from_id_salt("render-shader")
                    .selected_text(selected)
                    .width(170.0 * ps)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut emu.render.shader, String::new(), "None");
                        if let Some(cs) = &emu.cap_shader {
                            for n in cs.names() {
                                ui.selectable_value(&mut emu.render.shader, n.clone(), n.clone());
                            }
                        }
                    });
            });
        });
    }
}

/// The row label — Lacrima's rows are a left-hand label with the
/// control right-aligned on the same line.
fn row_label(ui: &mut egui::Ui, text: &str, ps: f32) {
    ui.label(
        RichText::new(text)
            .size(15.0 * ps)
            .color(Color32::from_gray(215)),
    );
}

/// Label | slider | "1.00" — Lacrima's Slider + value TextBlock row.
/// Double-click resets to `default` (Lacrima's DoubleTapped reset),
/// handled inside pill_slider's own interact so keyboard focus and
/// arrow-key nudging work exactly like the settings-page sliders.
fn render_slider(
    ui: &mut egui::Ui,
    label: &str,
    v: &mut f32,
    min: f32,
    max: f32,
    default: f32,
    ps: f32,
) {
    ui.horizontal(|ui| {
        row_label(ui, label, ps);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{v:.2}"))
                    .monospace()
                    .size(13.0 * ps)
                    .color(Color32::from_gray(160)),
            );
            let tw = (ui.available_width() - 8.0 * ps).max(40.0 * ps);
            let (track, _) = ui.allocate_exact_size(Vec2::new(tw, 12.0 * ps), Sense::hover());
            crate::pill_slider(
                ui,
                track,
                &format!("rs-{label}"),
                v,
                min,
                max,
                0.01,
                1.0,
                Some(default),
            );
        });
    });
    ui.add_space(12.0 * ps);
}

/// Label | switch + "On"/"Off" — Lacrima's ToggleSwitch row with the
/// state text beside the switch.
fn render_toggle(ui: &mut egui::Ui, label: &str, on: &mut bool, ps: f32) {
    ui.horizontal(|ui| {
        row_label(ui, label, ps);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(if *on { "On" } else { "Off" })
                    .size(13.0 * ps)
                    .color(Color32::from_gray(150)),
            );
            let (rect, _) = ui.allocate_exact_size(Vec2::new(46.0 * ps, 24.0 * ps), Sense::hover());
            crate::toggle_switch(ui, rect, &format!("rt-{label}"), on, 1.0);
        });
    });
    ui.add_space(12.0 * ps);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan Folder over a PS4 tree: the installed dump is added
    /// whole (never descended into), loose .pkg files are found —
    /// and a short alpha dir name ("Games") must not panic the
    /// worker via the title-id token scanner.
    #[test]
    fn ps4_scan_collects_installed_folders() {
        let base = std::env::temp_dir().join(format!("aes_scan_ps4_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("Games");
        let dump = root.join("CUSA12345 - Bloodborne");
        std::fs::create_dir_all(dump.join("sce_sys")).unwrap();
        std::fs::write(dump.join("sce_sys").join("param.sfo"), b"fake").unwrap();
        // A pkg INSIDE the dump must never surface as its own rom.
        std::fs::write(dump.join("patch.pkg"), b"x").unwrap();
        std::fs::write(root.join("Loose.pkg"), b"x").unwrap();
        let nested = root.join("Stuff").join("Inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.pkg"), b"x").unwrap();

        // Scanning "Games" exercises the short-name panic path: the
        // installed-folder check inspects each ancestor segment.
        let found = scan_rom_folder(&root, "PS4");
        assert!(found.contains(&dump));
        assert!(found.contains(&root.join("Loose.pkg")));
        assert!(found.contains(&nested.join("deep.pkg")));
        assert!(!found.contains(&dump.join("patch.pkg")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Narrow band: right-cluster slots that would reach into the
    /// centered pause button park in the left gap instead of
    /// overlapping it. Wide bands pack normally from the right.
    #[test]
    fn band_slots_relocate_around_pause() {
        let pause = Rect::from_center_size(Pos2::new(400.0, 70.0), Vec2::splat(80.0));
        // pause = 360..440, gap 10 -> intruding centers in (320, 480).
        // Wide band, left toggles at 80/190: nothing intrudes.
        let mut taken = vec![80.0, 190.0];
        let mut left = 300.0;
        let mut rx = 700.0;
        assert_eq!(band_slot(true, false, &mut rx, &mut left, &mut taken, pause, 1.0), 700.0);
        assert_eq!(band_slot(true, false, &mut rx, &mut left, &mut taken, pause, 1.0), 590.0);
        assert_eq!(left, 300.0);
        // Narrower band: 480 stays (its left edge is exactly at the
        // zone's end), 370 intrudes -> parks at left_next=300. The
        // next packed position 260 is clear of the zone but blocks
        // the parked 300; the left gap is full by then, so it keeps
        // its packed spot (degenerate ultra-narrow fallback).
        let mut taken = vec![80.0, 190.0];
        let mut left = 300.0;
        let mut rx = 480.0;
        assert_eq!(band_slot(true, false, &mut rx, &mut left, &mut taken, pause, 1.0), 480.0);
        assert_eq!(band_slot(true, false, &mut rx, &mut left, &mut taken, pause, 1.0), 300.0);
        assert_eq!(left, 410.0);
        assert_eq!(band_slot(true, false, &mut rx, &mut left, &mut taken, pause, 1.0), 260.0);
        // Absent slots never consume a position.
        assert!(band_slot(false, false, &mut rx, &mut left, &mut taken, pause, 1.0).is_nan());
        assert_eq!(rx, 150.0);
        // A slot landing on a left toggle relocates into the free gap:
        // pause at 450 -> zone 400..500, 490 intrudes -> parks at 300.
        let pause2 = Rect::from_center_size(Pos2::new(450.0, 70.0), Vec2::splat(80.0));
        let mut taken2 = vec![80.0, 190.0];
        let mut left2 = 300.0;
        let mut rx2 = 600.0;
        assert_eq!(band_slot(true, false, &mut rx2, &mut left2, &mut taken2, pause2, 1.0), 600.0);
        assert_eq!(band_slot(true, false, &mut rx2, &mut left2, &mut taken2, pause2, 1.0), 300.0);
        assert_eq!(left2, 410.0);
        // force_park relocates even a slot that would not intrude.
        let mut taken3 = vec![80.0, 190.0];
        let mut left3 = 300.0;
        let mut rx3 = 700.0;
        assert_eq!(
            band_slot(true, true, &mut rx3, &mut left3, &mut taken3, pause, 1.0),
            300.0
        );
    }

    /// The balance split: when the lanes flanking the pause hold the
    /// same icon count, the innermost right-cluster slots park left
    /// so the band reads symmetric (`spill_from` is the present-icon
    /// ordinal where spilling begins).
    #[test]
    fn band_spill_balances_icons_around_pause() {
        // 2 left (list + viewport) vs 4 right (power/tun/sliders/
        // layout, setup hidden by the session): with left room for
        // one, the innermost spills -> 3 left / 3 right.
        assert_eq!(band_spill_from(2, 4, 3, 1), 3);
        // More left room does not spill extra icons past the balance.
        assert_eq!(band_spill_from(2, 4, 3, 2), 3);
        // No left room at all: nothing spills.
        assert_eq!(band_spill_from(2, 4, 3, 0), 4);
        // 8 icons, two free left lanes: 4 / 4.
        assert_eq!(band_spill_from(2, 6, 4, 2), 4);
        // Right side already the smaller one: untouched.
        assert_eq!(band_spill_from(3, 2, 5, 3), 2);
        // Right lane capped by the pause zone: the excess spills.
        assert_eq!(band_spill_from(1, 4, 2, 3), 2);
        // No left room -> the capped right lane just overflows-packed
        // (degenerate, same as before balancing existed).
        assert_eq!(band_spill_from(1, 4, 2, 0), 4);
    }

    /// Play routing: the running game un-dismisses the viewport, a
    /// different game while a session runs queues the pending
    /// launch + close, and with no session Play launches directly —
    /// a double-click during capture must never just re-show the
    /// running game's viewport when the target differs.
    #[test]
    fn play_routes_pending_launch_for_another_game() {
        assert_eq!(play_route(true, true), PlayRoute::UnDismiss);
        assert_eq!(play_route(true, false), PlayRoute::QueuePending);
        assert_eq!(play_route(false, false), PlayRoute::Launch);
    }

    /// The emulation band's cover-layout cycles its own persisted key —
    /// the shared CoverLayoutMode regression: cycling one view used to
    /// switch the other. Emulation.CoverLayoutMode and
    /// Carousel.LayoutMode must serialize and round-trip independently.
    #[test]
    fn cover_layout_modes_are_independent() {
        let mut s = settings::Settings::default();
        s.emulation.cover_layout_mode = 2;
        s.carousel.layout_mode = 1;
        let json = serde_json::to_string(&s).unwrap();
        let mut back: settings::Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.emulation.cover_layout_mode, 2);
        assert_eq!(back.carousel.layout_mode, 1);
        // Cycling the emulation mode leaves the carousel's alone.
        back.emulation.cover_layout_mode = (back.emulation.cover_layout_mode + 1) % 3;
        assert_eq!(back.emulation.cover_layout_mode, 0);
        assert_eq!(back.carousel.layout_mode, 1);
    }

    /// Dismissing the capture viewport — the ⇄ switch's hide half,
    /// shared by the band button, Tab-to-cards during capture and
    /// Enter-opening another section mid-session — must drop
    /// game-fullscreen and the input tunnel: with no capture on
    /// screen both have nothing to drive.
    #[test]
    fn dismiss_viewport_drops_fullscreen_and_tunnel() {
        let mut emu = EmuView::new();
        emu.viewport_dismissed = false;
        emu.game_fs = true;
        emu.input_tun = true;
        emu.dismiss_viewport();
        assert!(emu.viewport_dismissed);
        assert!(!emu.game_fs);
        assert!(!emu.input_tun);
    }

    /// The ⇄ switch shared by the band button and Ctrl+S: hide
    /// clears fullscreen + tunnel, show just restores the capture.
    #[test]
    fn toggle_viewport_round_trips() {
        let mut emu = EmuView::new();
        emu.toggle_viewport();
        assert!(emu.viewport_dismissed);
        emu.toggle_viewport();
        assert!(!emu.viewport_dismissed);
        emu.game_fs = true;
        emu.input_tun = true;
        emu.toggle_viewport();
        assert!(emu.viewport_dismissed);
        assert!(!emu.game_fs);
        assert!(!emu.input_tun);
    }

    /// Capture fullscreen (double-click / context menu / Ctrl+E):
    /// entering while the list is showing jumps straight into the
    /// fullscreen capture — the flag alone does nothing while the
    /// viewport is dismissed. Exiting stays on the current surface.
    #[test]
    fn fullscreen_enter_unhides_viewport() {
        let mut emu = EmuView::new();
        emu.viewport_dismissed = true;
        emu.toggle_fullscreen();
        assert!(emu.game_fs);
        assert!(!emu.viewport_dismissed);
        emu.toggle_fullscreen();
        assert!(!emu.game_fs);
        assert!(!emu.viewport_dismissed);
    }

    /// The input tunnel (mouse emulation) — the band's pointer icon
    /// and Ctrl+M share this flip; the capture dismiss path (Esc
    /// aside) is what force-clears it.
    #[test]
    fn input_tunnel_toggles() {
        let mut emu = EmuView::new();
        emu.toggle_input_tunnel();
        assert!(emu.input_tun);
        emu.toggle_input_tunnel();
        assert!(!emu.input_tun);
    }

    /// Ctrl+Q / Esc's session branch: stopping with no session must be
    /// a clean no-op — the guard in `stop_session` is what keeps a
    /// stray keypress between teardown and relaunch from touching
    /// `pending_launch` or panicking on the absent session.
    #[test]
    fn stop_session_noops_without_session() {
        let mut emu = EmuView::new();
        emu.pending_launch = Some("rom.iso".into());
        emu.stop_session();
        assert!(emu.session.is_none());
        assert_eq!(
            emu.pending_launch.as_deref(),
            Some(std::path::Path::new("rom.iso"))
        );
    }

    /// The power button / "Close Emulator" menu item park a close
    /// request for the caller's confirm prompt — consumed exactly
    /// once so an answered request can't reopen the dialog.
    #[test]
    fn close_request_consumed_once() {
        let mut emu = EmuView::new();
        assert!(!emu.take_close_request());
        emu.request_close();
        assert!(emu.take_close_request());
        assert!(!emu.take_close_request());
    }
}
