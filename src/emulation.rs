//! The Emulation view prototype — Lacrima's EmulationView chrome:
//! `Grid RowDefinitions="*,Auto,Auto"` = the item view on top, the
//! 140px control band (section-list toggle left, cover-layout toggle
//! right), and the collapsible console-section strip at the bottom.
//! The item area is a placeholder until ROM scanning lands; the
//! settings tab renders the Emulation sections with the console cards
//! expandable but no handler controls wired yet.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Pos2, Rect, Sense,
    Stroke, StrokeKind, UiBuilder, Vec2,
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
    /// Extracted console-art dir — the strip tiles' fallback cover.
    art_dir: Option<PathBuf>,
    /// Last-seen `use_first_item_cover` — a settings flip re-themes
    /// every section tile (first ROM cover ⇄ console art).
    prev_first_cover: bool,
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
            art_dir,
            prev_first_cover: false,
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

    /// Launch `game` through the section's default handler — resolves
    /// the render window, hides it, and starts WGC capture on a
    /// worker thread.
    pub fn launch(&mut self, cfg: &settings::Emulation, game: &Path) {
        self.notice = None;
        let handler = self.active_handler(cfg);
        match handler.as_deref() {
            Some(XENIA_HANDLER) => {
                // Prefer a managed install (Download button) when the
                // configured path is empty or stale.
                let configured = cfg
                    .emulators
                    .get(XENIA_HANDLER)
                    .filter(|p| !p.trim().is_empty() && Path::new(p.trim()).exists());
                let exe = configured
                    .cloned()
                    .or_else(|| emu_dl::xenia_exe().map(|p| p.to_string_lossy().into_owned()));
                let Some(exe) = exe else {
                    self.notice = Some(
                        "No Xenia executable — set a path or press Download in Settings → Emulation → Xbox 360".into(),
                    );
                    return;
                };
                // LaunchEmulatorAsync: PrepareConfigForLaunch merges the
                // game's custom config onto the template before boot —
                // on a worker since the first run may download it.
                let (tx, rx) = mpsc::channel();
                let g = game.to_path_buf();
                std::thread::spawn(move || {
                    if let Some(root) = xenia_cfg::storage_root(Path::new(&exe).parent()) {
                        xenia_cfg::prepare_for_launch(&root, xenia_cfg::title_id(&g).as_deref());
                    }
                    let _ = tx.send(xenia::Session::start(&exe, &g));
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
                emu.session = Some(s);
                emu.launch_rx = None;
                emu.notice = None;
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
    // The fade finished — drop the stale frame texture.
    if emu.session.is_none() && emu.cap_t <= 0.0 {
        emu.frame_tex = None;
        emu.frame_seq = 0;
        emu.frame_buf = Vec::new();
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
            if ui.button("Edit Metadata").clicked() {
                open_editor = true;
                ui.close();
            }
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

    // Session controls — Lacrima's IsEmulatorRunning button set:
    // left adds the viewport toggle (switch list ⇄ game), the center
    // holds pause/play (process suspend/resume), and the right adds
    // the Close-Emulator power button at the far end.
    let mut toggle_viewport = false;
    let mut toggle_pause = false;
    let mut close_emu = false;
    if let Some(session) = &emu.session {
        // Viewport toggle — sits right of the album-list toggle
        // (StackPanel Spacing=50).
        let vp = Rect::from_center_size(
            Pos2::new(band.min.x + 50.0 * s + 60.0 * s + 50.0 * s + 30.0 * s, cy),
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
            Pos2::new(band.max.x - 50.0 * s - 30.0 * s, cy),
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
    let right_x = if emu.session.is_some() {
        band.max.x - 50.0 * s - 60.0 * s - 50.0 * s - 30.0 * s
    } else {
        band.max.x - 50.0 * s - 30.0 * s
    };
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
fn frame_dest_uv(rect: Rect, fw: usize, fh: usize, crop_t: f32) -> (Rect, Rect) {
    let src = fw as f32 / fh as f32;
    let fit = (rect.width() / fw as f32).min(rect.height() / fh as f32);
    let fit_dest = Rect::from_center_size(
        rect.center(),
        Vec2::new(fw as f32 * fit, fh as f32 * fit),
    );
    let dest = lerp_rect(fit_dest, rect, crop_t);
    let d = dest.width() / dest.height();
    let uw = (d / src).min(1.0);
    let uh = (src / d).min(1.0);
    let uv = Rect::from_min_max(
        Pos2::new(0.5 - uw / 2.0, 0.5 - uh / 2.0),
        Pos2::new(0.5 + uw / 2.0, 0.5 + uh / 2.0),
    );
    (dest, uv)
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
        if let Some(tex) = &emu.frame_tex {
            let [fw, fh] = tex.size();
            let (dest, uv) = frame_dest_uv(rect, fw, fh, emu.crop_t);
            painter.image(tex.id(), dest, uv, Color32::WHITE.gamma_multiply(alpha));
        }
        return;
    };
    let status = session.status();
    let game_name = session
        .game
        .file_stem()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "game".into());

    // Pull the newest captured frame into the egui texture — one
    // lock so dims/pixels can't tear, cloned only when `seq` bumps.
    // Skipped while the layer is fully faded out (user is browsing
    // the list) so hidden sessions don't burn a full-frame memcpy +
    // texture upload per frame.
    if alpha > 0.02 {
        // Swap the frame into `frame_buf` — the slot gets the old
        // buffer back so the capture thread's scratch<->slot
        // ping-pong stays allocation-free, and the UI pays exactly
        // one conversion copy into egui's pixels.
        let mut slot = session.frames().lock().unwrap_or_else(|p| p.into_inner());
        if slot.seq != emu.frame_seq
            && slot.w > 0
            && slot.h > 0
            && slot.rgba.len() == slot.w * slot.h * 4
        {
            let (w, h) = (slot.w, slot.h);
            std::mem::swap(&mut slot.rgba, &mut emu.frame_buf);
            let seq = slot.seq;
            drop(slot);
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
            emu.frame_seq = seq;
        }
    }

    // Ease the aspect-fill blend — the crop/letterbox glides instead
    // of snapping when the destination aspect crosses the ~12% fill
    // threshold (strip/band resizes, fullscreen transitions).
    let mut dbg_want_dt = None;
    if let Some(tex) = &emu.frame_tex {
        let [fw, fh] = tex.size();
        let src = fw as f32 / fh as f32;
        // `want` uses the TARGET aspect, not the animating rect's —
        // otherwise it flips as the easing aspect sweeps through the
        // fill band mid-transition and crop_t reverses (the bounce).
        let dst = target.width() / target.height();
        let want = ((src / dst - 1.0).abs() < 0.12) as u8 as f32;
        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
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

    if let Some(tex) = &emu.frame_tex {
        let [fw, fh] = tex.size();
        let (dest, uv) = frame_dest_uv(rect, fw, fh, emu.crop_t);
        painter.image(tex.id(), dest, uv, Color32::WHITE.gamma_multiply(alpha));
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
