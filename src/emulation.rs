//! The Emulation view prototype — Lacrima's EmulationView chrome:
//! `Grid RowDefinitions="*,Auto,Auto"` = the item view on top, the
//! 140px control band (section-list toggle left, cover-layout toggle
//! right), and the collapsible console-section strip at the bottom.
//! The item area is a placeholder until ROM scanning lands; the
//! settings tab renders the Emulation sections with the console cards
//! expandable but no handler controls wired yet.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Pos2, Rect, RichText,
    Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};

use crate::{
    album_row, carousel, emu_dl, emu_meta, library, paint, rom_editor, search_pill, settings,
    transport, xenia, xenia_cfg, xenia_overlay,
};

/// Section keys that have a playable handler — only Xenia/X360 is
/// wired; the other cards stay browse/collect placeholders.
const XENIA_SECTION: &str = "X360";
const XENIA_HANDLER: &str = "xenia";

/// Lacrima's EmulationConsoleCatalog FilePatterns — what a section's
/// handler accepts (the handler *is* the file-type filter). Archives
/// are appended to every section like `GetScanPatterns` does.
fn section_patterns(key: &str) -> &'static [&'static str] {
    match key {
        "SNES" => &["sfc", "smc", "fig", "swc"],
        "NES" => &["nes", "fds", "unf", "unif"],
        "N64" => &["n64", "z64", "v64"],
        "DC" => &["cdi", "gdi", "chd", "cue", "iso"],
        "GCN" => &["iso", "gcm", "ciso", "gcz", "rvz", "wia", "dol", "elf", "tgc"],
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
        // Steam's AllFilePatterns — the empty set accepts anything.
        "STEAM" => &[],
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
    ("STEAM", "Steam", "Steam"),
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
    ("3DS", include_bytes!("../assets/consoles/3DS.png")),
    ("Arcade", include_bytes!("../assets/consoles/Arcade.png")),
    ("Dreamcast", include_bytes!("../assets/consoles/Dreamcast.png")),
    ("FBN", include_bytes!("../assets/consoles/FBN.png")),
    ("GBA", include_bytes!("../assets/consoles/GBA.png")),
    ("GC", include_bytes!("../assets/consoles/GC.png")),
    ("Genesis", include_bytes!("../assets/consoles/Genesis.png")),
    ("N64", include_bytes!("../assets/consoles/N64.png")),
    ("NES", include_bytes!("../assets/consoles/NES.png")),
    ("PS2", include_bytes!("../assets/consoles/PS2.png")),
    ("PS3", include_bytes!("../assets/consoles/PS3.png")),
    ("PS4", include_bytes!("../assets/consoles/PS4.png")),
    ("PSP", include_bytes!("../assets/consoles/PSP.png")),
    ("PSX", include_bytes!("../assets/consoles/PSX.png")),
    ("SNES", include_bytes!("../assets/consoles/SNES.png")),
    ("Saturn", include_bytes!("../assets/consoles/Saturn.png")),
    ("Steam", include_bytes!("../assets/consoles/Steam.png")),
    ("Switch", include_bytes!("../assets/consoles/Switch.png")),
    ("Wii", include_bytes!("../assets/consoles/Wii.png")),
    ("WiiU", include_bytes!("../assets/consoles/WiiU.png")),
    ("X360", include_bytes!("../assets/consoles/X360.png")),
    ("Xbox", include_bytes!("../assets/consoles/Xbox.png")),
];

/// Extract the embedded console art into `<data>/Assets/Consoles/`
/// (only missing/stale files are rewritten) — returns the dir.
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
    pub meta_rxs: Vec<mpsc::Receiver<emu_meta::RomInfo>>,
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
    /// Last frame seq uploaded into `frame_tex`.
    pub frame_seq: u64,
    /// Persistent frame staging buffer — swapped with the capture
    /// slot so neither side allocates per frame.
    frame_buf: Vec<u8>,
    /// One-line notice for the item area (launch errors, hints).
    pub notice: Option<String>,
    /// In-flight emulator download (Xenia install/update) — the
    /// settings card polls the shared progress.
    pub dl: Option<emu_dl::Shared>,
    /// Last download result line shown under the progress bar.
    pub dl_msg: Option<String>,
    /// The ROM metadata editor overlay (EmulationMetadataOverlay).
    pub editor: rom_editor::RomEditor,
    /// Xenia per-game Patches overlay (XeniaPatchesOverlay).
    pub patches: xenia_overlay::PatchesOverlay,
    /// Xenia per-game Custom Config overlay (XeniaCustomConfigOverlay).
    pub cfg_ov: xenia_overlay::ConfigOverlay,
    /// Launch pipeline — config prep + Session::start run on a
    /// worker; the result lands here (Lacrima's LaunchEmulatorAsync
    /// Task.Run shape so template downloads never stall the UI).
    pub launch_rx: Option<mpsc::Receiver<Result<xenia::Session, String>>>,
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
    pub cap_shader: Option<crate::cap_shader::CaptureShader>,
    /// The imported zero-copy ring — frames are sampled straight from
    /// the D3D11 shared textures (Lacrima's GPU interop); `None` puts
    /// us on the CPU-mirror path (same as Lacrima's fallback).
    gpu_cap: Option<crate::cap_shader::ImportedRing>,
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
    /// Whether the saved aspect_key was applied to this session's
    /// window (the HWND resolves asynchronously after launch).
    aspect_applied: bool,
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
            frame_seq: 0,
            frame_buf: Vec::new(),
            notice: None,
            dl: None,
            dl_msg: None,
            editor: rom_editor::RomEditor::new(),
            patches: xenia_overlay::PatchesOverlay::new(),
            cfg_ov: xenia_overlay::ConfigOverlay::new(),
            launch_rx: None,
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
            aspect_applied: false,
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
        self.sections
            .sort_by_key(|s| rank.get(section_key(s).as_str()).copied().unwrap_or(usize::MAX));
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
            if let Some(sec) = self
                .sections
                .iter_mut()
                .find(|s| section_key(s) == key)
            {
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
        console_art(self.art_dir.as_deref(), &key).into_iter().next()
    }

    /// Load a section's persisted games into the card view and spawn
    /// the metadata/cover resolver (idempotent on re-select).
    pub fn open_section(&mut self, i: usize, cfg: &settings::Emulation) {
        let key = section_key(&self.sections[i]);
        if self.loaded_key.as_deref() == Some(key.as_str()) {
            return;
        }
        self.loaded_key = Some(key.clone());
        // Per-section render options (Lacrima's SectionSettings) —
        // the panel edits this live copy.
        self.render = cfg.render.get(&key).cloned().unwrap_or_default();
        self.items = cfg
            .games
            .get(&key)
            .map(|g| g.iter().map(PathBuf::from).collect())
            .unwrap_or_default();
        self.sections[i].tracks = self.items.clone();
        self.cards.open_roms(&self.sections[i], &rom_cover);
        self.meta_rxs.push(emu_meta::spawn_resolver(
            key,
            self.sections[i].title.clone(),
            self.items.clone(),
        ));
        self.refresh_section_covers(cfg);
        self.notice = None;
    }

    /// Resolved titles + fetched covers from the worker — applied to
    /// the card items as they stream in.
    fn drain_meta(&mut self, cfg: &settings::Emulation) {
        let mut i = 0;
        let mut covers_landed = false;
        while i < self.meta_rxs.len() {
            let mut closed = false;
            loop {
                match self.meta_rxs[i].try_recv() {
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
            self.meta_rxs.push(emu_meta::spawn_resolver(key, platform, added));
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

    /// The handler id that launches the loaded section — Lacrima's
    /// `SelectedHandlerId` (the section's default handler), falling
    /// back to the one wired handler for X360.
    pub fn active_handler(&self, cfg: &settings::Emulation) -> Option<String> {
        let key = self.loaded_key.as_deref()?;
        if let Some(h) = cfg.default_handlers.get(key) {
            return Some(h.clone());
        }
        (key == XENIA_SECTION).then(|| XENIA_HANDLER.to_string())
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
            let valid = |p: &str| {
                !p.trim().is_empty() && Path::new(p.trim()).exists()
            };
            self.setup_exe = if valid(&configured) {
                Some(PathBuf::from(configured.trim()))
            } else {
                (handler == XENIA_HANDLER)
                    .then(emu_dl::xenia_exe)
                    .flatten()
            };
        }
        self.setup_exe.clone()
    }

    /// Launch `game` through the section's default handler — resolves
    /// the render window, hides it, and starts WGC capture on a
    /// worker thread.
    pub fn launch(&mut self, cfg: &settings::Emulation, game: &Path) {
        self.notice = None;
        let handler = self.active_handler(cfg);
        match handler.as_deref() {
            Some(XENIA_HANDLER) => {
                let Some(exe) = self.setup_exe(cfg) else {
                    self.notice = Some(
                        "No Xenia executable — set a path or press Download in Settings → Emulation → Xbox 360".into(),
                    );
                    return;
                };
                // Per-section capture pool depth (Lacrima's
                // LowLatencyCapture → FramePoolSize 1 vs 2).
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
                    if let Some(root) = xenia_cfg::storage_root(exe.parent()) {
                        xenia_cfg::prepare_for_launch(&root, xenia_cfg::title_id(&g).as_deref());
                    }
                    let _ = tx.send(xenia::Session::start(
                        &exe.to_string_lossy(),
                        &g,
                        low_latency,
                    ));
                });
                self.launch_rx = Some(rx);
                self.notice = Some("Preparing Xenia…".into());
            }
            _ => self.notice = Some("No emulator handler configured for this section".into()),
        }
    }

    /// Ask the running session to close the emulator.
    pub fn stop_session(&mut self) {
        if let Some(s) = &self.session {
            s.stop();
        }
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
    cover_layout: &mut u8,
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

    // The overlays own the pointer while one is up — cards, the
    // section strip and the band stop receiving input.
    let interactive = interactive
        && !emu.editor.is_open()
        && !emu.patches.is_open()
        && !emu.cfg_ov.is_open();

    // Async launch — the worker merged the custom config and spawned
    // Xenia; the session lands here.
    if let Some(rx) = &emu.launch_rx {
        match rx.try_recv() {
            Ok(Ok(s)) => {
                // ReloadArcadeLockedPillarboxCropFromMetadata — the
                // per-ROM locked crop wins over auto-detection.
                emu.pillar_lock = crate::emu_meta::locked_crop(&s.game);
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
                emu.notice = Some(e);
                emu.launch_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => ui.ctx().request_repaint(),
            Err(mpsc::TryRecvError::Disconnected) => emu.launch_rx = None,
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

    // Session teardown once the emulator reports exit. The last
    // captured frame stays mounted so the presentation layer can fade
    // out over the carousel instead of snapping away (Lacrima's
    // CapturePresentationOpacity DoubleTransition).
    if emu.session.as_mut().is_some_and(|s| s.finished()) {
        emu.session = None;
        emu.viewport_dismissed = false;
        emu.game_fs = false;
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
        r.brightness
            .store(emu.render.brightness.to_bits(), Relaxed);
        r.saturation
            .store(emu.render.saturation.to_bits(), Relaxed);
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
    emu.drain_meta(cfg);

    // The carousel stays mounted under the capture layer — cards keep
    // receiving input only once the layer is mostly faded out. While
    // the layer is fully opaque the cards aren't painted at all.
    if emu.active.is_some() && cap_a < 0.999 {
        draw_items(
            ui,
            upper,
            emu,
            cfg,
            car_cfg,
            *cover_layout,
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
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
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
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
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
    // Hide the OS cursor while the game owns the screen; it comes
    // back the moment fullscreen exits (any path).
    if fs != emu.cursor_hidden {
        emu.cursor_hidden = fs;
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::CursorVisible(!fs));
    }
    if emu.cap_t > 0.0 {
        // `stable` is the destination the slide is heading TO — the
        // fullscreen content rect, or `upper` at list_t's endpoint.
        // Fit/fill decisions key off this so nothing mid-transition
        // can flip the crop state.
        let stable = if fs { content } else { settled_upper };
        draw_session(ui, &painter, pres_rect, stable, emu, alpha * cap_a, s);
    }
    // Gestures on the capture layer (Lacrima's CaptureContextMenuLayer
    // + double-click): double-click toggles game-fullscreen, right-
    // click opens the session menu. The interact region also stops
    // clicks from reaching the cards underneath.
    if emu.session.is_some() && cap_a > 0.5 {
        let cap = ui.interact(
            pres_rect,
            ui.id().with("emu-capture-layer"),
            Sense::click(),
        );
        if cap.double_clicked() {
            emu.game_fs = !emu.game_fs;
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
                let (l, r) = (emu.pillar_cur.0.round() as u32, emu.pillar_cur.1.round() as u32);
                let fw = emu
                    .frame_tex
                    .as_ref()
                    .map(|t| t.size()[0] as u32)
                    .unwrap_or(0);
                if let Some(session) = &emu.session {
                    if (l > 0 || r > 0) && fw > 0 {
                        crate::emu_meta::save_locked_crop(&session.game, l, r, fw);
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
                        crate::emu_meta::clear_locked_crop(&session.game);
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
                emu.game_fs = !emu.game_fs;
                ui.close();
            }
            if ui.button("Close Emulator").clicked() {
                stop = true;
                ui.close();
            }
        });
        if open_editor {
            let g = emu.session.as_ref().unwrap().game.clone();
            emu.editor.open(ui.ctx(), g, String::new());
        }
        if stop {
            if let Some(session) = &emu.session {
                session.stop();
            }
        }
    }
    // In game-fullscreen the chrome (band, strip, search, overlays'
    // underlying view) is not painted at all.
    let chrome_hidden = fs && cap_a > 0.5;

    if !chrome_hidden {
    // Row 1 — the 140px control band (Background #E6000000 plus the
    // top Separator that every Lacrima chrome band draws).
    painter.rect_filled(band, 0.0, Color32::from_black_alpha((230.0 * alpha) as u8));
    let line = Stroke::new(
        s.max(1.0),
        Color32::from_white_alpha((51.0 * alpha) as u8),
    );
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
    // the outer slots: [layout][setup][sliders][power], shrinking to
    // [layout][setup] while no capture runs (the capture-settings
    // toggle only exists while a game is being captured).
    let mut rx = band.max.x - 50.0 * s - 30.0 * s;
    let mut slot = |cond: bool| -> f32 {
        let x = rx;
        if cond {
            rx -= 110.0 * s;
            x
        } else {
            f32::NAN
        }
    };
    let pw_x = slot(emu.session.is_some());
    let sliders_x = slot(emu.session.is_some());
    let setup_x = rx;
    let layout_x = rx - 110.0 * s;

    // Capture-settings toggle (Lacrima's ToggleRenderOptions) — the
    // sliders glyph; only exists while a session captures.
    if !sliders_x.is_nan() {
        let rb = Rect::from_center_size(
            Pos2::new(sliders_x, cy),
            Vec2::splat(60.0 * s),
        );
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

    // Handler-setup launch (LaunchCurrentSectionHandlerSetup) —
    // spawns the configured emulator with no game so the user can
    // reach its own settings. Disabled while a session runs or a
    // launch is in flight (Lacrima's CanLaunch… guard).
    let setup_exe = emu.setup_exe(cfg);
    let setup_ok =
        setup_exe.is_some() && emu.session.is_none() && emu.launch_rx.is_none();
    let sb = Rect::from_center_size(
        Pos2::new(setup_x, cy),
        Vec2::splat(60.0 * s),
    );
    let r = if interactive && setup_ok {
        ui.interact(sb, ui.id().with("emu-setup"), Sense::click())
    } else {
        ui.interact(sb, ui.id().with("emu-setup"), Sense::hover())
    }
    .on_hover_text(if setup_ok {
        "Open Emulator Settings".to_string()
    } else {
        "Open Emulator Settings — unavailable while a game runs".to_string()
    });
    if r.clicked() {
        if let Some(exe) = &setup_exe {
            if let Err(e) = xenia::launch_setup(&exe.to_string_lossy()) {
                emu.notice = Some(e);
            }
        }
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    // The handler's own exe icon (ExtractIconEx → 48px) inside a
    // rounded-rect button like Lacrima's setup tile — dimmed while
    // unavailable, a gear glyph when extraction fails.
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
        Stroke::new(
            s.max(1.0),
            Color32::from_white_alpha((40.0 * icon_a) as u8),
        ),
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

    // Session controls — Lacrima's IsEmulatorRunning button set:
    // left adds the viewport toggle (switch list ⇄ game), the center
    // holds pause/play (process suspend/resume), and the right adds
    // the Close-Emulator power button at the far end.
    let mut toggle_viewport = false;
    let mut toggle_pause = false;
    let mut close_emu = false;
    if let Some(session) = &emu.session {
        // Viewport toggle — directly right of the albums toggle
        // (same 110px button spacing the right cluster uses).
        let vp = Rect::from_center_size(
            Pos2::new(band.min.x + 190.0 * s, cy),
            Vec2::splat(60.0 * s),
        );
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
        let pp = Rect::from_center_size(band.center(), Vec2::splat(80.0 * s));
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
        let pw = Rect::from_center_size(
            Pos2::new(pw_x, cy),
            Vec2::splat(60.0 * s),
        );
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
        emu.viewport_dismissed = !emu.viewport_dismissed;
        if emu.viewport_dismissed {
            emu.game_fs = false;
        }
    }
    if toggle_pause {
        if let Some(session) = &emu.session {
            session.set_paused(!session.is_paused());
        }
    }
    if close_emu {
        if let Some(session) = &emu.session {
            session.stop();
        }
    }

    // Right: the cover-layout cycle (Margin 0,0,50,0) — Lacrima binds
    // the same global CoverLayoutMode the music band uses. While a
    // session runs it makes room for the power button (Lacrima's
    // right StackPanel order: layout … power … volume).
    let right_x = layout_x;
    let btn = Rect::from_center_size(Pos2::new(right_x, cy), Vec2::splat(60.0 * s));
    let r = if interactive {
        ui.interact(btn, ui.id().with("emu-layout"), Sense::click())
    } else {
        ui.interact(btn, ui.id().with("emu-layout"), Sense::hover())
    }
    .on_hover_text("Cover Layout");
    if r.clicked() {
        *cover_layout = (*cover_layout + 1) % 3;
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    transport::icon_layout(&painter, btn, *cover_layout, alpha);

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
            album_row::Action::Opened(i) => {
                emu.active = Some(i);
                // Load the section's persisted games into the card
                // view on first activation — the key travels on the
                // tile's `emu:` dir so reordering can't miskey it.
                emu.open_section(i, cfg);
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
                emu.active = emu
                    .sections
                    .iter()
                    .position(|s| section_key(s) == key);
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
}

/// Overlay header title — the resolved/custom title when present,
/// else the cleaned file stem.
fn rom_title(p: &Path) -> String {
    emu_meta::cached_title(p)
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
    let launcher = configured
        .filter(|p| p.exists())
        .or_else(emu_dl::xenia_exe);
    xenia_cfg::storage_root(launcher.as_deref().and_then(|p| p.parent()))
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
    let fit_dest = Rect::from_center_size(
        rect.center(),
        Vec2::new(fhf * eff * fit, fhf * fit),
    );
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
) {
    // Opaque backing — never a compositor hole, matching the
    // #FF000000 Border behind Lacrima's capture host.
    painter.rect_filled(rect, 0.0, Color32::BLACK.gamma_multiply(alpha));

    let Some(session) = &emu.session else {
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
        let (dest, uv) = frame_dest_uv(
            rect,
            fw,
            fh,
            emu.crop_t,
            emu.render.stretch,
            emu.pillar_cur,
        );
        let seq = emu.frame_seq;
        let adjust = render_adjust(emu);
        let cb = emu
            .gpu_cap
            .as_ref()
            .map(|gc| (gc.bgs[(seq % 3) as usize].clone(), gc.fence.clone(), gc.size))
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
            if emu.gpu_cap.as_ref().map(|g| g.gen) != Some(share.gen)
                && emu.gpu_failed_gen != share.gen
            {
                match emu.cap_shader.as_mut().map(|cs| cs.import_share(&share)) {
                    Some(Ok(r)) => {
                        emu.gpu_cap = Some(r);
                        emu.gpu_failed_gen = 0;
                        emu.gpu_err = None;
                    }
                    Some(Err(e)) => {
                        emu.gpu_failed_gen = share.gen;
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
        .store(emu.render.remove_pillars || emu.gpu_cap.is_none(), Ordering::Relaxed);

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
        if !gpu_live || emu.render.remove_pillars {
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
                        pixels: bytemuck::cast_slice::<u8, egui::Color32>(&emu.frame_buf)
                            .to_vec(),
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
            if emu.render.remove_pillars {
                let det = if let Some((ll, lr, lfw)) = emu.pillar_lock {
                    crate::pillarbox::scale_locked(ll, lr, lfw, w as u32)
                } else {
                    crate::pillarbox::resolve(
                        w as u32,
                        crate::pillarbox::detect(
                            &emu.frame_buf,
                            w,
                            h,
                            crate::pillarbox::Profile::Aggressive,
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
        let eff = ((fw as f32 - emu.pillar_tgt.0 as f32 - emu.pillar_tgt.1 as f32)
            .max(1.0))
            / fh as f32;
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
        if !emu.render.remove_pillars && (emu.pillar_tgt != (0, 0) || emu.pillar_cur != (0.0, 0.0))
        {
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
        let (dest, uv) = frame_dest_uv(
            rect,
            fw,
            fh,
            emu.crop_t,
            emu.render.stretch,
            emu.pillar_cur,
        );
        // The zero-copy ring drives the draw when imported — the
        // newest signaled slot is sampled in place after the D3D12
        // queue waits on the cross-API fence. Otherwise the CPU
        // mirror path: named shader via PaintCallback or plain blit.
        let seq = emu.frame_seq;
        let adjust = render_adjust(emu);
        let cb = if let Some((bg, fence, size)) = emu
            .gpu_cap
            .as_ref()
            .map(|gc| (gc.bgs[(seq % 3) as usize].clone(), gc.fence.clone(), gc.size))
        {
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
        } else {
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
        };
        match cb {
            Some(cb) => {
                // Must be the egui_wgpu::Callback wrapper — the
                // renderer downcasts the Arc to that exact type; a
                // bare PaintCallback shape is silently culled.
                painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
                    dest, cb,
                ));
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
            (emu.frame_seq == 0).then(|| format!("Waiting for frames — {game_name}"))
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
}

/// Lacrima's PortalCaptureStatsOverlay — a #C0000000 rounded panel
/// anchored bottom-left of the capture (Margin 16,16,16,24), whole
/// panel at RenderOverlayOpacity. Content: capture fps + frametime,
/// a status line, optional GPU renderer/vendor, and the 520x40
/// frametime polyline with 16.7/33.3 ms guides.
fn draw_stats_overlay(
    painter: &egui::Painter,
    rect: Rect,
    emu: &mut EmuView,
    alpha: f32,
    s: f32,
) {
    let op = (emu.render.overlay_opacity * alpha).clamp(0.0, 1.0);
    if op <= 0.01 {
        return;
    }
    let gpu = if emu.render.gpu_info {
        Some(
            emu.gpu_desc
                .get_or_insert_with(|| {
                    crate::wgc::gpu_desc()
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
            Stroke::new(1.5 * s, Color32::from_rgb(0, 0xFF, 0xFF).gamma_multiply(op * 0.7)),
        ));
    }
}

/// The item area with a section loaded and no session: the shared
/// `Carousel` widget in ROM mode — the audio view's exact list (the
/// band's layout icon drives the same CoverLayoutMode, so carousel /
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
    emu.cards.xenia_menu = key == XENIA_SECTION
        && emu.active_handler(cfg).as_deref() == Some(XENIA_HANDLER);
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
    match action {
        // One session at a time — Play while a game runs in the
        // background just brings the capture viewport back.
        carousel::Action::Play(p) => {
            if emu.session.is_some() {
                emu.viewport_dismissed = false;
            } else {
                emu.launch(cfg, &p);
            }
        }
        // "Edit Metadata" — the EmulationMetadataOverlay port.
        carousel::Action::Edit(p) => {
            let platform = emu
                .sections
                .iter()
                .find(|c| section_key(c) == key)
                .map(|c| c.title.clone())
                .unwrap_or_default();
            emu.editor.open(ui.ctx(), p, platform);
        }
        // Xenia per-game overlays — the storage root resolves the
        // same way a launch does (launcher dir ⇄ managed install).
        carousel::Action::Patches(p) => {
            emu.patches.open(&p, &rom_title(&p), xenia_storage(cfg));
        }
        carousel::Action::CustomConfig(p) => {
            emu.cfg_ov.open(&p, &rom_title(&p), xenia_storage(cfg));
        }
        // Confirmed deletes come back through `pending_delete` —
        // main.rs runs them through the shared dialog.
        carousel::Action::Delete(paths) => emu.pending_delete = Some(paths),
        carousel::Action::Clear => emu.clear_games(cfg),
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
                emu.add_games(cfg, scan_rom_folder(&dir, &key));
            }
        }
        _ => {}
    }
}

/// ScanFolder — walk the picked folder recursively for the section's
/// handler patterns (plus the archive extensions Lacrima always adds).
fn scan_rom_folder(root: &Path, key: &str) -> Vec<PathBuf> {
    let pats = section_patterns(key);
    let accept = |ext: &str| {
        // STEAM's AllFilePatterns — no extension gate at all.
        key == "STEAM"
            || pats.iter().any(|p| p.eq_ignore_ascii_case(ext))
            || ARCHIVE_EXTS.contains(&ext)
    };
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
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
        // X360's body hosts the Xenia handler row — a bit taller than
        // the empty-state cards.
        let body_h = if open {
            if key == XENIA_SECTION {
                196.0 * s
            } else {
                74.0 * s
            }
        } else {
            0.0
        };
        let card = Rect::from_min_size(
            Pos2::new(x, y),
            Vec2::new(right - x, head_h + body_h),
        );
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
            if key == XENIA_SECTION {
                // Xenia handler row — Lacrima's EmulationHandlerAppItem:
                // name, launcher path, then the right-aligned
                // Set-default/Browse/Clear/Download row, with the
                // download progress block underneath.
                let m = 12.0 * s;
                let ry = inner.min.y + m;
                let font14 = FontId::proportional(14.0 * s);
                painter.text(
                    Pos2::new(inner.min.x + m, ry + 2.0 * s),
                    Align2::LEFT_TOP,
                    "Xenia",
                    FontId::proportional(18.0 * s),
                    whitesmoke.gamma_multiply(t),
                );
                // Installed/version hint next to the name.
                if let Some(exe) = emu_dl::xenia_exe() {
                    painter.text(
                        Pos2::new(inner.min.x + m + 64.0 * s, ry + 7.0 * s),
                        Align2::LEFT_TOP,
                        exe.parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        FontId::proportional(12.0 * s),
                        darkgray.gamma_multiply(t * 0.8),
                    );
                }

                // Path TextBox — Lacrima shows the launcher path; ours
                // is directly editable too.
                let box_rect = Rect::from_min_size(
                    Pos2::new(inner.min.x + m, ry + 32.0 * s),
                    Vec2::new(inner.width() - m * 2.0, 26.0 * s),
                );
                let entry = cfg.emulators.entry(XENIA_HANDLER.to_string()).or_default();
                let mut tui = cui.new_child(
                    UiBuilder::new()
                        .max_rect(box_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                tui.set_opacity(t);
                tui.add(
                    egui::TextEdit::singleline(entry)
                        .desired_width(box_rect.width())
                        .hint_text(
                            egui::RichText::new("Path to xenia.exe")
                                .font(font14.clone()),
                        )
                        .font(font14.clone()),
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

                // Right-aligned button row — Lacrima's
                // Set default | Browse | Clear | Download.
                let downloading = emu.dl.is_some();
                let is_default = cfg
                    .default_handlers
                    .get(XENIA_SECTION)
                    .is_some_and(|h| h == XENIA_HANDLER);
                let has_path = cfg
                    .emulators
                    .get(XENIA_HANDLER)
                    .is_some_and(|p| !p.trim().is_empty());
                let btn_font = FontId::proportional(14.0 * s);
                let measure = |p: &egui::Painter, text: &str| {
                    p.layout_no_wrap(text.to_owned(), btn_font.clone(), Color32::WHITE)
                        .rect
                        .width()
                        + 30.0 * s
                };
                let by = ry + 68.0 * s;
                let mut bx = inner.max.x - m;

                // Download (green #1E6B3A).
                let w = measure(&painter, "Download");
                bx -= w;
                let (dl_clicked, _) = crate::flat_button(
                    &mut cui,
                    Pos2::new(bx, by),
                    "Download",
                    s,
                    !downloading,
                    Color32::from_rgb(0x1E, 0x6B, 0x3A),
                    Color32::WHITE,
                    t,
                );
                if dl_clicked {
                    emu.dl = Some(emu_dl::download_xenia());
                    emu.dl_msg = None;
                }
                bx -= 12.0 * s;

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
                        cfg.emulators.remove(XENIA_HANDLER);
                        emu.settings_dirty = true;
                    }
                    bx -= 12.0 * s;
                }

                // Browse.
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
                        .set_title("Locate xenia.exe")
                        .add_filter("Xenia", &["exe"])
                        .pick_file()
                    {
                        cfg.emulators
                            .insert(XENIA_HANDLER.into(), file.to_string_lossy().into_owned());
                        emu.settings_dirty = true;
                    }
                }
                bx -= 12.0 * s;

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
                            .insert(XENIA_SECTION.into(), XENIA_HANDLER.into());
                        emu.settings_dirty = true;
                    }
                }

                // Progress block (Lacrima's IsDownloading row) + the
                // last result line.
                let py = by + 38.0 * s;
                let mut status_line = emu.dl_msg.clone();
                if let Some(dl) = &emu.dl {
                    let (stage, frac, done) = {
                        let p = dl.lock().unwrap_or_else(|e| e.into_inner());
                        (p.stage.clone(), p.frac, p.done.clone())
                    };
                    if let Some(res) = done {
                        match res {
                            Ok(exe) => {
                                cfg.emulators.insert(XENIA_HANDLER.into(), exe);
                                emu.settings_dirty = true;
                                status_line = Some("Xenia installed".into());
                            }
                            Err(e) => status_line = Some(e),
                        }
                        emu.dl = None;
                    } else {
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
                }
                emu.dl_msg = status_line.clone();
                if let Some(msg) = status_line {
                    // Fixed slot above the hint — ~14px of clear
                    // margin between the status and the hint text.
                    painter.text(
                        Pos2::new(inner.min.x + m, inner.max.y - 36.0 * s),
                        Align2::LEFT_BOTTOM,
                        msg,
                        FontId::proportional(13.0 * s),
                        Color32::from_gray(0xCC).gamma_multiply(t),
                    );
                }
                // Launch hint — where games come from.
                painter.text(
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
    let slide = ui
        .ctx()
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
    let panel = Rect::from_min_max(
        Pos2::new(area.max.x - w * t, area.min.y),
        area.max,
    );
    let painter = ui.painter().clone();
    painter.rect_filled(
        panel,
        0.0,
        Color32::from_black_alpha((242.0 * alpha) as u8),
    );
    painter.vline(
        panel.min.x,
        panel.y_range(),
        Stroke::new(
            s.max(1.0),
            Color32::from_white_alpha((51.0 * alpha) as u8),
        ),
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
            let (xr, resp) =
                ui.allocate_exact_size(Vec2::splat(26.0 * ps), Sense::click());
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
            ui.painter().line_segment(
                [
                    c + Vec2::new(-k, -k),
                    c + Vec2::new(k, k),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    c + Vec2::new(k, -k),
                    c + Vec2::new(-k, k),
                ],
                stroke,
            );
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
                    ["Uniform", "Uniform to Fill", "Fill"]
                        [emu.render.stretch.min(2) as usize],
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
    render_toggle(pui, "Remove side pillars", &mut emu.render.remove_pillars, ps);
    pui.add_space(6.0 * ps);
    render_toggle(pui, "Show Stats Overlay", &mut emu.render.stats_overlay, ps);
    render_toggle(pui, "Show Frametime Graph", &mut emu.render.frametime_graph, ps);
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
                                ui.selectable_value(
                                    &mut emu.render.shader,
                                    n.clone(),
                                    n.clone(),
                                );
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
            let (track, _) =
                ui.allocate_exact_size(Vec2::new(tw, 12.0 * ps), Sense::hover());
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
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(46.0 * ps, 24.0 * ps), Sense::hover());
            crate::toggle_switch(ui, rect, &format!("rt-{label}"), on, 1.0);
        });
    });
    ui.add_space(12.0 * ps);
}
