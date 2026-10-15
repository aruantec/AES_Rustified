#![windows_subsystem = "windows"]

mod album_row;
mod apl;
mod audio;
mod cap_shader;
mod pillarbox;
mod carousel;
mod edge_border;
mod emu_dl;
mod emu_meta;
mod exe_icon;
mod emulation;
mod equalizer;
mod icons;
mod library;
mod logo;
mod meta_cache;
mod paint;
mod particles;
mod rom_editor;
mod xenia_cfg;
mod xenia_overlay;
mod settings;
mod shadertoy;
mod search_pill;
mod spectrum_bar;
mod tag_editor;
mod transport;
mod url_prompt;
mod waveform_bar;
mod wgc;
mod widgets;
mod xenia;
// dead_code: a few query helpers (full -J metadata, extractor
// profiles beyond Default) are the surface for future video-stream
// selection — Lacrima's OnlineStreamSelector shape.
#[allow(dead_code)]
mod ytdlp;

use eframe::egui_wgpu;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, PointerButton, Pos2,
    Rect, ResizeDirection, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2, ViewportCommand,
};

const CORNER_RADIUS: u8 = 10;
/// MainTopBar's declared height (virtual px, rendered at 1/ScaleFactor).
const TITLE_BAR_HEIGHT: f32 = 70.0;
const RESIZE_BORDER: f32 = 8.0;
/// Seconds for one full fade-out -> fade-in view transition.
const NAV_FADE_SECS: f32 = 0.55;
/// Height of the albums strip at the bottom of the Music view
/// (Lacrima's MusicListView Height=266), scaled by ui_scale().
const ALBUM_ROW_H: f32 = 266.0;
const ALBUM_ROW_MARGIN_BOTTOM: f32 = 10.0;

fn album_row_rect(content: Rect, s: f32) -> Rect {
    let h = ALBUM_ROW_H * s;
    let margin = ALBUM_ROW_MARGIN_BOTTOM * s;
    Rect::from_min_max(
        Pos2::new(content.min.x, content.max.y - margin - h),
        Pos2::new(content.max.x, content.max.y - margin),
    )
}



/// Merge the persisted virtual albums into the list (deduped by dir).
/// Created and .apl-imported albums are the same record — neither is a
/// filesystem result, so waiting for scan completion left their slots
/// empty the whole walk.
fn merge_virtual_albums(albums: &mut Vec<library::Album>, lib: &settings::Library) {
    for ia in &lib.virtual_albums {
        if albums.iter().any(|a| a.dir.to_string_lossy() == ia.dir) {
            continue;
        }
        let urls = lib.url_items.get(&ia.dir).cloned().unwrap_or_default();
        let mut a = library::Album::virtual_album(&ia.dir, ia.title.clone(), &urls);
        normalize_scanned_album(lib, &mut a);
        albums.push(a);
    }
}

/// Apply the persisted per-album overlays to a freshly scanned album:
/// url cover layers + count (the scanner can't see url items), the
/// Rename Album display-title override, and the deleted-track
/// exclusion so card deletions survive rescans.
fn normalize_scanned_album(lib: &settings::Library, a: &mut library::Album) {
    let dir = a.dir.to_string_lossy().into_owned();
    let urls = lib.url_items.get(&dir).cloned().unwrap_or_default();
    // Folder albums' scan covers carry no url thumbs — merge persisted
    // url items into the fan layers.
    let web: Vec<library::CoverSrc> = urls
        .iter()
        .filter_map(|u| library::CoverSrc::web_for(u))
        .collect();
    a.sync_url_covers(web, urls.len());
    if let Some(t) = lib.album_titles.get(&dir) {
        a.title = t.clone();
    }
    if let Some(ex) = lib.excluded_tracks.get(&dir) {
        a.tracks
            .retain(|t| !ex.iter().any(|s| s == t.to_string_lossy().as_ref()));
    }
}



/// Smoothstep ease-in-out: slow start and end, no hard velocity changes.
fn ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Emulation,
    Music,
    Recomp,
}

impl View {
    /// Order matches the first three main menu entries; the gear icon
    /// opens the settings overlay instead of navigating.
    const SUB_VIEWS: [View; 3] = [View::Emulation, View::Recomp, View::Music];

    fn name(self) -> &'static str {
        match self {
            View::Main => "Main",
            View::Emulation => "Emulation",
            View::Music => "Music",
            View::Recomp => "Re/Decompilation",
        }
    }
}

/// View navigation with a fade-out/fade-in transition: the current view
/// fades out (0 -> 0.5), then the target fades in (0.5 -> 1.0).
enum Nav {
    Settled(View),
    Fading { from: View, to: View, t: f32 },
}

impl Nav {
    fn alpha(&self, v: View) -> f32 {
        match self {
            Nav::Settled(cur) => (v == *cur) as u8 as f32,
            Nav::Fading { from, to, t } => {
                if *t < 0.5 {
                    if v == *from { 1.0 - ease(t * 2.0) } else { 0.0 }
                } else if v == *to {
                    ease((t - 0.5) * 2.0)
                } else {
                    0.0
                }
            }
        }
    }

    /// The view we're heading to (used for the back button).
    fn target(&self) -> View {
        match self {
            Nav::Settled(v) => *v,
            Nav::Fading { to, .. } => *to,
        }
    }

    fn request(&mut self, v: View) {
        if self.target() == v {
            return;
        }
        let from = match self {
            Nav::Settled(cur) => *cur,
            Nav::Fading { from, to, t } => {
                if *t > 0.5 { *to } else { *from }
            }
        };
        *self = Nav::Fading { from, to: v, t: 0.0 };
    }

    fn advance(&mut self, dt: f32) {
        if let Nav::Fading { to, t, .. } = self {
            *t += dt / NAV_FADE_SECS;
            if *t >= 1.0 {
                *self = Nav::Settled(*to);
            }
        }
    }
}

/// Boot timing — stage timestamps appended to
/// `<data_root>/boot.log` (truncated each launch) so startup
/// regressions are diagnosable instead of guessed at.
static BOOT_T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub(crate) fn boot_mark(stage: &str) {
    let (Some(t0), Some(dir)) = (BOOT_T0.get(), settings::data_root()) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("boot.log"))
    {
        use std::io::Write;
        let _ = writeln!(
            f,
            "{:>7.1}ms  {}",
            t0.elapsed().as_secs_f64() * 1000.0,
            stage
        );
    }
}

fn main() -> eframe::Result<()> {
    let _ = BOOT_T0.set(std::time::Instant::now());
    if let Some(dir) = settings::data_root() {
        let _ = std::fs::write(dir.join("boot.log"), "");
    }
    boot_mark("main enter");
    // Crash forensics — a panic anywhere (UI thread, capture worker,
    // FFI-adjacent paths) dumps its message + backtrace to
    // <data_root>/crash.log before the process dies. Keeps silent
    // kills diagnosable.
    std::panic::set_hook(Box::new(|info| {
        let mut msg = format!(
            "[{}] panic: {}\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            info
        );
        msg.push_str(&format!("{}\n", std::backtrace::Backtrace::capture()));
        if let Some(dir) = settings::data_root() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
                .map(|mut f| {
                    use std::io::Write;
                    let _ = f.write_all(msg.as_bytes());
                });
        }
        eprintln!("{msg}");
    }));
    // Program.cs startup: append the per-user Tools dir to PATH so
    // spawned tools (yt-dlp and its helpers) are always locatable.
    {
        let tools = ytdlp::tools_dir();
        let _ = std::fs::create_dir_all(&tools);
        let path = std::env::var("PATH").unwrap_or_default();
        if !path
            .to_ascii_lowercase()
            .contains(&tools.to_string_lossy().to_ascii_lowercase() as &str)
        {
            let sep = if cfg!(windows) { ";" } else { ":" };
            std::env::set_var("PATH", format!("{path}{sep}{}", tools.display()));
        }
    }
    let mut settings = settings::load();
    boot_mark("settings loaded");
    // DX12 only — the zero-copy WGC interop imports D3D11 shared
    // textures through wgpu's DX12 backend; on Vulkan the import
    // silently can't happen and capture falls back to the CPU
    // mirror. Everything else (egui, shadertoys, capture shaders)
    // runs fine on DX12.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(cn) = &mut wgpu_options.wgpu_setup {
        cn.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }

    let native_options = eframe::NativeOptions {
        wgpu_options,
        viewport: egui::ViewportBuilder::default()
            .with_title("AES Rustified")
            // Taskbar/alt-tab icon (the file icon comes from app.rc).
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/AES.png"))
                    .expect("embedded app icon"),
            )
            .with_inner_size(Vec2::new(settings.window.width, settings.window.height))
            .with_min_inner_size(Vec2::new(360.0, 260.0))
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };

    boot_mark("run_native enter");
    eframe::run_native(
        "AES Rustified",
        native_options,
        Box::new(move |cc| {
            boot_mark("creation ctx (window+wgpu device ready)");
            if let Some(rs) = &cc.wgpu_render_state {
                let i = rs.adapter.get_info();
                boot_mark(&format!(
                    "wgpu adapter: {} backend={:?} type={:?}",
                    i.name, i.backend, i.device_type
                ));
            }
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            // Lacrima's first-run default (MainWindow.axaml.cs): when no
            // ScaleFactor was ever persisted, seed it from the display
            // DPI scale, clamped to 1.0..=2.0.
            if !settings.scale_factor_persisted {
                let ppp = cc
                    .egui_ctx
                    .input(|i| i.viewport().native_pixels_per_point)
                    .unwrap_or(1.0);
                settings.ui.scale_factor = ppp.clamp(1.0, 2.0);
            }
            Ok(Box::new(AesRustApp::new(cc, settings)))
        }),
    )
}

/// The usable screen rect (work area — minus taskbar/docked bars) in
/// egui points. Windows answers via `SPI_GETWORKAREA`, which returns
/// the primary monitor's work area in physical pixels; other platforms
/// fall back to egui's full `monitor_size`.
fn work_area_rect(ctx: &egui::Context) -> Option<Rect> {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct RcWork {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        extern "system" {
            fn SystemParametersInfoW(
                ui_action: u32,
                ui_param: u32,
                pv_param: *mut RcWork,
                win_ini: u32,
            ) -> i32;
        }
        const SPI_GETWORKAREA: u32 = 0x0030;
        let mut rc = RcWork {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // Physical px -> egui points: viewport positions/sizes are all
        // in points (OuterPosition multiplies by pixels_per_point).
        if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rc, 0) } != 0 {
            let ppp = ctx.pixels_per_point();
            return Some(Rect::from_min_max(
                Pos2::new(rc.left as f32 / ppp, rc.top as f32 / ppp),
                Pos2::new(rc.right as f32 / ppp, rc.bottom as f32 / ppp),
            ));
        }
    }
    ctx.input(|i| i.viewport().monitor_size)
        .map(|m| Rect::from_min_size(Pos2::ZERO, m))
}

struct AesRustApp {
    icons: icons::MenuIcons,
    menu_scales: [f32; 4],
    centered: bool,
    shadertoy: Option<shadertoy::Shadertoy>,
    logo: logo::TitleLogo,
    lacrima: particles::LacrimaParticles,
    clock: widgets::ClockWidget,
    /// The turntable widget (Lacrima's PlayerCompositionControl): spinning
    /// cover disc, circular seek ring, play/pause tonearm.
    player_widget: widgets::PlayerWidget,
    /// The playing track's cover as an egui texture — the vinyl label.
    playing_cover: Option<egui::TextureHandle>,
    widget_edit: bool,
    settings: settings::Settings,
    nav: Nav,
    settings_open: bool,
    settings_tab: usize,
    last_save: std::time::Instant,
    albums: Vec<library::Album>,
    album_row: album_row::AlbumRow,
    album_rx: Option<std::sync::mpsc::Receiver<library::Album>>,
    /// Dirs seen in the in-flight scan — on completion, list entries
    /// absent here (folders deleted from disk since the last scan)
    /// are dropped instead of lingering forever.
    scan_seen: std::collections::HashSet<String>,
    /// Created lazily on first play — errors (no output device) just
    /// leave it None.
    player: Option<audio::AudioPlayer>,
    waveform: Vec<f32>,
    /// Damped copy of `waveform` — the bar paints this so progressive
    /// generation snapshots ease in instead of popping.
    waveform_shown: Vec<f32>,
    waveform_rx: Option<std::sync::mpsc::Receiver<Vec<f32>>>,
    /// Flag the previous generation checks so stale results are dropped.
    waveform_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    waveform_loading: bool,
    waveform_bar: waveform_bar::WaveformBar,
    /// Trailing-silence length of the playing track in seconds,
    /// measured off the completed waveform (Lacrima's
    /// _trailingSilenceSeconds — 0 disables the early advance).
    trailing_silence_s: f64,
    /// When playback first entered the trailing-silence region — the
    /// SilenceAdvanceDelayMs countdown before the track ends early.
    silence_entered: Option<std::time::Instant>,
    transport: transport::Transport,
    spectrum: spectrum_bar::SpectrumBar,
    /// Dominant colors of the playing album's cover — the bar gradient
    /// when Spectrum.CoverColors is on (Lacrima's
    /// UseMusicSpectrumCoverColors -> GetDominantColorGradient).
    spectrum_colors: Option<Vec<Color32>>,
    search: search_pill::SearchPill,
    album_search: search_pill::SearchPill,
    /// The loaded album's track view (Lacrima's CoverItems +
    /// CompositionCoverControl — carousel / card grid modes).
    carousel: carousel::Carousel,
    /// Tag metadata arriving for the loaded album's tracks.
    track_info_rx: Option<std::sync::mpsc::Receiver<Vec<library::TrackInfo>>>,
    /// "Edit Metadata" modal overlay (Lacrima's OpenMetadataCommand).
    tag_editor: tag_editor::TagEditor,
    /// Optional yt-dlp tool managed from Settings > Libraries —
    /// Lacrima's YtDlpManager (Tools-dir install/update/uninstall).
    ytdlp: ytdlp::YtDlp,
    /// "Add Url"/"Add Playlist" prompt overlay (Lacrima's
    /// IsAddUrlPopupOpen / IsAddPlaylistPopupOpen popups).
    url_prompt: url_prompt::UrlPrompt,
    /// yt-dlp stream resolution for a pending URL-item play —
    /// Lacrima's ResolveMediaSourceAsync -> PlayFile hop. Carries the
    /// item path so stale results (user clicked away meanwhile) drop.
    resolve_rx:
        Option<std::sync::mpsc::Receiver<(std::path::PathBuf, Result<String, String>)>>,
    /// Expanded playlist member urls arriving from the page scrape.
    playlist_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    /// Online metadata for freshly added url items (pairs with path).
    url_info_rx:
        Option<std::sync::mpsc::Receiver<Vec<(std::path::PathBuf, library::TrackInfo)>>>,
    /// Now-playing cover decoded on a worker — url items fetch over
    /// http, so the decode can't happen on the UI thread.
    playing_cover_rx: Option<
        std::sync::mpsc::Receiver<(std::path::PathBuf, Option<egui::ColorImage>)>,
    >,
    /// Opened album's top cover decoded on a worker — pairs with the
    /// album dir so a stale result (user opened another album) drops.
    /// Web covers fetch over the network; decoding them on the UI
    /// thread froze the whole view for seconds.
    album_cover_rx:
        Option<std::sync::mpsc::Receiver<(String, Option<egui::ColorImage>)>>,
    /// Path currently loaded in the player — drives the playing-item
    /// glow and the top-left track info overlay.
    playing_track: Option<std::path::PathBuf>,
    /// Which view owns the current selection — the albums strip or
    /// the loaded album's cards. The Delete key routes here only, so
    /// a stale highlight in the other view can never delete
    /// (Lacrima's per-ListBox selection scope).
    sel_owner: SelOwner,
    /// Alt-tap detection for the cover-layout cycle — Alt is a
    /// modifier, not a Key event, so the handler watches its edge
    /// and requires a clean press→release (no other key, window
    /// focused) so Alt+Tab can never fire it.
    alt_down: bool,
    alt_clean: bool,
    /// Switch-style "exit fullscreen?" confirmation — see
    /// `FsExitPrompt`.
    fs_exit: FsExitPrompt,
    /// Item/album delete confirmation — same dialog style.
    del_prompt: DeletePrompt,
    /// Emulation view state — section strip, item search, active
    /// section (prototype; ROM items land later).
    emu: emulation::EmuView,
    started: std::time::Instant,
}

/// Selection owner for Delete-key routing — set by whichever view
/// last saw a user selection gesture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelOwner {
    None,
    Albums,
    Cards,
}

/// The Switch-style exit-fullscreen confirmation: a translucent veil
/// over the live content and a centered dialog. Esc again or Cancel
/// closes it (staying fullscreen); Exit/Enter confirms.
#[derive(Default)]
struct FsExitPrompt {
    open: bool,
    /// 0 = Exit, 1 = Cancel — Tab/arrows toggle, hover follows.
    sel: usize,
    /// Painted fade 0..1 — trails `open` so the veil and dialog fade
    /// OUT instead of snapping off when the prompt closes.
    vis: f32,
    /// Exit confirmed — fullscreen leaves once the fade completes.
    pending_exit: bool,
}

impl FsExitPrompt {
    fn begin(&mut self, ctx: &egui::Context) {
        self.sel = 0;
        self.open = true;
        self.pending_exit = false;
        ctx.request_repaint();
    }
}

/// What a delete confirmation guards — re-injected as the original
/// action on confirm so the handlers run unchanged.
enum PendingDelete {
    /// Carousel items (DeletePointedItem / selection Delete).
    Items(Vec<std::path::PathBuf>),
    /// Album-row dirs (DeleteFolder / selection Delete).
    Albums(Vec<String>),
    /// Emulation ROM items (the rom-mode card menu / Delete key).
    Roms(Vec<std::path::PathBuf>),
}

/// The same Switch-style confirm dialog for deletes — the request is
/// stashed in `pending` while open; on confirm it lands in
/// `confirmed` and is consumed next frame.
#[derive(Default)]
struct DeletePrompt {
    open: bool,
    sel: usize,
    vis: f32,
    pending: Option<PendingDelete>,
    confirmed: Option<PendingDelete>,
}

impl DeletePrompt {
    fn request(&mut self, pending: PendingDelete, ctx: &egui::Context) {
        self.sel = 1; // safe default: Cancel focused
        self.pending = Some(pending);
        self.open = true;
        ctx.request_repaint();
    }

    fn dismiss(&mut self) {
        self.open = false;
        self.pending = None;
    }
}

impl AesRustApp {
    fn new(cc: &eframe::CreationContext<'_>, settings: settings::Settings) -> Self {
        // Lacrima renders its title in Consolas Bold and widgets in Segoe UI
        // Light; load both from the OS and fall back to built-in fonts if
        // unavailable.
        let mut defs = egui::FontDefinitions::default();
        let mut load_font = |name: &str, path: &str, family: &str| {
            std::fs::read(path)
                .map(|data| {
                    defs.font_data.insert(
                        name.to_owned(),
                        std::sync::Arc::new(egui::FontData::from_owned(data)),
                    );
                    defs.families.insert(
                        egui::FontFamily::Name(family.into()),
                        vec![name.to_owned()],
                    );
                })
                .is_ok()
        };
        let have_font = load_font(
            "consolas-bold",
            r"C:\Windows\Fonts\consolab.ttf",
            logo::CONSOLAS_BOLD_FAMILY,
        );
        let have_light_font = load_font(
            "segoe-light",
            r"C:\Windows\Fonts\segoeuil.ttf",
            widgets::SEGOE_LIGHT_FAMILY,
        );
        // Glyph fallback: egui's built-in fonts lack Hangul/Kanji/Kana
        // and Arabic, so those track titles draw as boxes. Appending
        // Windows fonts to the Proportional family makes missing
        // glyphs fall through per-character (system font fallback).
        // Note: egui has no bidi/shaping — Arabic renders LTR with
        // unjoined forms; legible, but not properly shaped.
        let mut have_cjk = false;
        for (name, path) in [
            ("malgun-gothic", r"C:\Windows\Fonts\malgun.ttf"),
            ("ms-yahei", r"C:\Windows\Fonts\msyh.ttc"),
            ("yu-gothic", r"C:\Windows\Fonts\YuGothR.ttc"),
            ("segoe-ui", r"C:\Windows\Fonts\segoeui.ttf"),
            ("tahoma", r"C:\Windows\Fonts\tahoma.ttf"),
        ] {
            if let Ok(data) = std::fs::read(path) {
                defs.font_data.insert(
                    name.to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(data)),
                );
                defs.families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push(name.to_owned());
                have_cjk = true;
            }
        }
        if have_font || have_light_font || have_cjk {
            cc.egui_ctx.set_fonts(defs);
        }
        boot_mark("fonts loaded");
        let mut transport = transport::Transport::new();
        transport.cover_layout = settings.carousel.layout_mode.min(2);

        // Seed the row from the persisted manifest (last session's
        // display order incl. virtual keys): virtual albums build for
        // real, scanned dirs become placeholders — title + default art
        // — that the streamed scan replaces in place. The row renders
        // instantly in final order instead of filling as folders walk.
        let lib = &settings.library;
        let mut initial_albums: Vec<library::Album> = lib
            .cached_albums
            .iter()
            .filter(|d| !lib.excluded_albums.contains(*d))
            .map(|d| {
                if let Some(ia) =
                    lib.virtual_albums.iter().find(|ia| ia.dir == *d)
                {
                    // Virtual album — real record (its position in the
                    // manifest restores interleaved ordering exactly).
                    let urls =
                        lib.url_items.get(&ia.dir).cloned().unwrap_or_default();
                    let mut a = library::Album::virtual_album(
                        &ia.dir,
                        ia.title.clone(),
                        &urls,
                    );
                    normalize_scanned_album(lib, &mut a);
                    a
                } else {
                    // Scanned dir — placeholder tile until the stream
                    // replaces it in place.
                    let mut a = library::Album::placeholder(d);
                    normalize_scanned_album(lib, &mut a);
                    a
                }
            })
            .collect();
        // Virtual albums imported since the manifest was written aren't
        // in it — append them (also covers zero music roots, where no
        // scan ever runs).
        merge_virtual_albums(&mut initial_albums, lib);

        let mut app = Self {
            icons: icons::load(&cc.egui_ctx),
            menu_scales: [1.0; 4],
            centered: false,
            shadertoy: cc.wgpu_render_state.as_ref().map(|rs| {
                let mut toy = shadertoy::Shadertoy::new(rs);
                toy.select(&settings.effects.shadertoy);
                toy
            }),
            logo: logo::TitleLogo::new(have_font),
            lacrima: particles::LacrimaParticles::new(),
            // Widget geometry: offset is the widget CENTER as a
            // fraction of the content rect (centered stays centered at
            // any window size); size is absolute virtual units so
            // widgets keep their size. Sizes <= 4.0 are legacy
            // fractional saves — fall back to the default layout once.
            clock: widgets::ClockWidget::new(have_light_font, {
                let c = &settings.clock;
                (c.left >= -0.5
                    && c.top >= -0.5
                    && c.left <= 1.5
                    && c.top <= 1.5
                    && c.width > 4.0
                    && c.height > 4.0)
                    .then(|| {
                        (
                            Vec2::new(c.left, c.top),
                            Vec2::new(c.width, c.height),
                            c.visible,
                        )
                    })
            }),
            // Same split restore for the turntable. A negative or
            // out-of-range Player.Left means "unplaced"/legacy ->
            // Lacrima's default layout.
            player_widget: widgets::PlayerWidget::new({
                let p = &settings.player;
                (p.left >= 0.0
                    && p.top >= -0.5
                    && p.left <= 1.5
                    && p.top <= 1.5
                    && p.width > 4.0
                    && p.height > 4.0)
                    .then(|| {
                        (
                            Vec2::new(p.left, p.top),
                            Vec2::new(p.width, p.height),
                            p.visible,
                        )
                    })
            }),
            playing_cover: None,
            widget_edit: false,
            album_rx: if settings.library.music_roots.is_empty() {
                None
            } else {
                Some(library::spawn_scan(&settings.library.music_roots))
            },
            scan_seen: std::collections::HashSet::new(),
            settings,
            nav: Nav::Settled(View::Main),
            settings_open: false,
            settings_tab: 0,
            last_save: std::time::Instant::now(),
            albums: initial_albums,
            album_row: album_row::AlbumRow::new(),
            player: None,
            waveform: Vec::new(),
            waveform_shown: Vec::new(),
            waveform_rx: None,
            waveform_cancel: None,
            waveform_loading: false,
            waveform_bar: waveform_bar::WaveformBar::new(),
            trailing_silence_s: 0.0,
            silence_entered: None,
            transport,
            spectrum: spectrum_bar::SpectrumBar::new(),
            spectrum_colors: None,
            search: search_pill::SearchPill::default(),
            album_search: search_pill::SearchPill::default(),
            carousel: carousel::Carousel::new(),
            track_info_rx: None,
            tag_editor: tag_editor::TagEditor::new(),
            ytdlp: ytdlp::YtDlp::new(),
            url_prompt: url_prompt::UrlPrompt::new(),
            resolve_rx: None,
            playlist_rx: None,
            url_info_rx: None,
            playing_cover_rx: None,
            album_cover_rx: None,
            playing_track: None,
            sel_owner: SelOwner::None,
            alt_down: false,
            alt_clean: false,
            fs_exit: FsExitPrompt::default(),
            del_prompt: DeletePrompt::default(),
            emu: emulation::EmuView::new(),
            started: std::time::Instant::now(),
        };
        boot_mark("app struct built");
        // Capture-shader pipelines live on the GPU — created once with
        // the wgpu render state like the shadertoy background.
        app.emu.cap_shader = cc
            .wgpu_render_state
            .as_ref()
            .map(cap_shader::CaptureShader::new);
        boot_mark("cap_shader ready");
        // Lacrima kicks off RefreshYtDlpInfo at startup so the
        // Libraries tab opens with populated state (worker thread).
        app.ytdlp.ensure_queried();
        // Restore persisted per-section game lists onto the tiles.
        app.emu.seed_games(&app.settings.emulation);
        boot_mark("app::new done");
        app
    }
}

impl eframe::App for AesRustApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        {
            static FRAME: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            let n = FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 3 || n == 30 || n == 120 {
                boot_mark(&format!("frame {n} built"));
            }
        }
        // Center the window once, before it is shown — on the work
        // area (screen minus the taskbar), not the full monitor.
        if !self.centered {
            if let Some(wa) = work_area_rect(ui.ctx()) {
                let size = ui.max_rect().size();
                ui.ctx().send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(
                    wa.min.x + (wa.width() - size.x) / 2.0,
                    wa.min.y + (wa.height() - size.y) / 2.0,
                )));
                self.centered = true;
            }
        }

        // The shader paints first so everything else (frame stroke, menu)
        // lands on top of it. Painted every frame, so always repaint.
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        let is_fullscreen = ui
            .ctx()
            .input(|i| i.viewport().fullscreen.unwrap_or(false));
        // Mirror the current window size into settings (un-maximized only,
        // so the restored size is the normal one).
        if !maximized && !is_fullscreen {
            if let Some(inner) = ui.ctx().input(|i| i.viewport().inner_rect) {
                self.settings.window.width = inner.width();
                self.settings.window.height = inner.height();
            }
        }

        // Global UI scale — Lacrima's ScaleFactor/ScalableDecorator: the
        // content tree is laid out in a virtual space `scale`x the window
        // and rendered at 1/scale, so interior metrics divide by the
        // factor (higher value = smaller, denser UI). Quantized to 0.05
        // so scaled font sizes stay bounded while the slider drags.
        let scale =
            ((self.settings.ui.scale_factor.clamp(1.0, 3.0) * 20.0).round() / 20.0).max(1.0);
        let s = 1.0 / scale;
        // Fullscreen hides the title bar — the content covers the
        // whole screen.
        let bar_h = if is_fullscreen {
            0.0
        } else {
            TITLE_BAR_HEIGHT * s
        };

        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
        let elapsed = ui.ctx().input(|i| i.time) as f32;
        // Cold-start fade: a black veil over the whole window lifting
        // over ~0.6s so the first frame doesn't pop.
        let boot_t = (self.started.elapsed().as_secs_f32() / 0.6).min(1.0);
        self.nav.advance(dt);
        if matches!(self.nav, Nav::Fading { .. }) {
            // Nothing else repaints while idle — without this the
            // crossfade stalls until the next OS event arrives.
            ui.ctx().request_repaint();
        }
        self.logo.advance(dt, "AES Rustified");

        let radius = if maximized || is_fullscreen {
            0
        } else {
            CORNER_RADIUS
        };
        let frame_rect = ui.max_rect();
        let content = Rect::from_min_max(
            Pos2::new(frame_rect.min.x, frame_rect.min.y + bar_h),
            frame_rect.max,
        );

        // Opaque backdrop under the content area: the window itself is
        // transparent, and while the shader fades out the view below must
        // not bleed through to the desktop.
        ui.painter().rect_filled(
            content,
            CornerRadius {
                nw: 0,
                ne: 0,
                sw: radius,
                se: radius,
            },
            Color32::BLACK,
        );

        let alpha_main = self.nav.alpha(View::Main);
        if let Some(toy) = &mut self.shadertoy {
            if alpha_main > 0.001 && self.settings.effects.show_shadertoy {
                ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                    frame_rect,
                    toy.callback(f32::from(radius), alpha_main),
                ));
                // The shader animates — keep the frame loop going.
                ui.ctx().request_repaint();
            }
        }

        // The back button is part of each view's chrome, so it crossfades
        // exactly like the content: out with the outgoing view during the
        // first half, in with the incoming view during the second.
        let back_alpha = match &self.nav {
            Nav::Settled(View::Main) => 0.0,
            Nav::Settled(_) => 1.0,
            Nav::Fading { from, to, t } => {
                if *t < 0.5 {
                    if *from == View::Main {
                        0.0
                    } else {
                        1.0 - ease(t * 2.0)
                    }
                } else if *to == View::Main {
                    0.0
                } else {
                    ease((t - 0.5) * 2.0)
                }
            }
        };
        let mut back_clicked = false;
        let mut esc_back = false;
        // Set by the title-bar's fullscreen-exit button — consumed
        // after the frame to open the exit confirmation.
        let mut fs_exit_req = false;
        let mut gear_clicked = false;

        // Album scan results stream in on a background thread — each
        // album lands the moment its directory walk finishes, so the
        // row fills tile by tile instead of staying empty until the
        // whole scan completes. Deleted albums stay excluded (their
        // dirs are in Settings.Library.ExcludedAlbums); the per-album
        // merge normalizes url covers, title overrides, and removed
        // tracks before the tile is inserted into its sorted slot.
        if self.album_rx.is_some() {
            let rx = self.album_rx.as_ref().unwrap();
            let mut done = false;
            // Cap the per-frame drain so tiles visibly fill in during
            // a big scan rather than landing in one burst.
            for _ in 0..32 {
                match rx.try_recv() {
                    Ok(mut a) => {
                        let dir = a.dir.to_string_lossy().into_owned();
                        self.scan_seen.insert(dir.clone());
                        if self
                            .settings
                            .library
                            .excluded_albums
                            .contains(&dir)
                        {
                            continue;
                        }
                        normalize_scanned_album(&self.settings.library, &mut a);
                        // Append in discovery order while scanning —
                        // the sorted insert made late-list tiles pop in
                        // ahead of un-arrived front ones (arrival order
                        // can't know final positions mid-scan). The
                        // real order applies once, when the walk ends.
                        if let Some(pos) =
                            self.albums.iter().position(|x| x.dir == a.dir)
                        {
                            self.albums[pos] = a;
                        } else {
                            self.albums.push(a);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
            if done {
                // A rescan replaces the list album by album — entries
                // whose dirs never arrived (folder deleted from disk,
                // or now excluded) drop out. Virtual albums aren't
                // discovered by the filesystem scan — keep and
                // re-append them from settings (a dragged position is
                // honored too).
                let imported = self.settings.library.virtual_albums.clone();
                self.albums.retain(|a| {
                    let dir = a.dir.to_string_lossy();
                    self.scan_seen.contains(dir.as_ref())
                        || imported.iter().any(|ia| ia.dir == dir)
                });
                merge_virtual_albums(&mut self.albums, &self.settings.library);
                // Scan-complete ordering: saved drag rank first, then
                // root grouping + alphabetical — near-identity when the
                // discovery walk already arrived alpha-sorted.
                let rank: std::collections::HashMap<&str, usize> = self
                    .settings
                    .library
                    .album_order
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.as_str(), i))
                    .collect();
                let roots = self.settings.library.music_roots.clone();
                self.albums.sort_by_key(|a| {
                    let dir = a.dir.to_string_lossy();
                    (
                        rank.get(dir.as_ref()).copied().unwrap_or(usize::MAX),
                        roots
                            .iter()
                            .position(|r| a.dir.starts_with(r))
                            .unwrap_or(usize::MAX),
                        a.title.to_lowercase(),
                    )
                });
                // Persist the final order as next session's manifest —
                // the row then renders instantly in this order while
                // the rescan streams real albums into their slots.
                self.settings.library.cached_albums = self
                    .albums
                    .iter()
                    .map(|a| a.dir.to_string_lossy().into_owned())
                    .collect();
                settings::save(&self.settings);
                self.album_rx = None;
            }
            ui.ctx().request_repaint();
        }
        // Waveform buckets land the same way (one decode per track).
        // The worker streams partial snapshots while generating — drain
        // them all and keep the latest; the channel closing marks done.
        if let Some(rx) = &self.waveform_rx {
            loop {
                match rx.try_recv() {
                    Ok(w) => self.waveform = w,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.waveform_rx = None;
                        self.waveform_loading = false;
                        // Generation finished — measure the trailing
                        // silence for SilenceAdvanceDelayMs. Partial
                        // snapshots would read the un-filled tail as
                        // silence, so this waits for completion like
                        // Lacrima's analysisComplete guard.
                        let dur = self
                            .player
                            .as_ref()
                            .map(|p| p.duration().as_secs_f64())
                            .unwrap_or(0.0);
                        self.trailing_silence_s =
                            trailing_silence_seconds(&self.waveform, dur);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        // Display-side smoothing: buckets damp toward the latest
        // snapshot so streamed generation sweeps in smoothly (and a
        // cache hit gets a quick grow-in rather than a pop).
        if self.waveform_shown.len() != self.waveform.len() {
            self.waveform_shown = vec![0.0; self.waveform.len()];
        }
        if !self.waveform.is_empty() {
            let k = 1.0 - (-14.0 * dt).exp();
            let mut settled = true;
            for (s, &t) in self.waveform_shown.iter_mut().zip(self.waveform.iter()) {
                *s += (t - *s) * k;
                if (t - *s).abs() > 0.003 {
                    settled = false;
                }
            }
            if !settled {
                ui.ctx().request_repaint();
            }
        }

        let Self {
            icons,
            menu_scales,
            logo,
            lacrima,
            clock,
            player_widget,
            playing_cover,
            widget_edit,
            settings,
            nav,
            settings_open,
            settings_tab,
            last_save,
            albums,
            album_row,
            album_rx,
            player,
            waveform,
            waveform_shown,
            waveform_rx,
            waveform_cancel,
            waveform_loading,
            waveform_bar,
            trailing_silence_s,
            silence_entered,
            transport,
            spectrum,
            spectrum_colors,
            search,
            album_search,
            carousel,
            track_info_rx,
            tag_editor,
            ytdlp,
            url_prompt,
            resolve_rx,
            playlist_rx,
            url_info_rx,
            playing_cover_rx,
            album_cover_rx,
            scan_seen,
            playing_track,
            sel_owner,
            alt_down,
            alt_clean,
            fs_exit,
            del_prompt,
            emu,
            shadertoy,
            ..
        } = self;
        // yt-dlp worker results (install/update/version checks feed the
        // Libraries tab).
        if ytdlp.pump() {
            ui.ctx().request_repaint();
        }
        // URL-item stream resolved — open the direct stream url
        // (Lacrima: OpenMediaItemAsync -> ResolveMediaSourceAsync ->
        // PlayFile; the worker already tried bestaudio then best).
        // Stale results for an item that stopped being current drop.
        if let Some(rx) = resolve_rx.as_ref() {
            match rx.try_recv() {
                Ok((path, Ok(stream))) => {
                    if playing_track.as_ref() == Some(&path) {
                        if player.is_none() {
                            *player = audio::AudioPlayer::new().ok();
                            if let Some(p) = player {
                                for (i, &db) in
                                    settings.equalizer.bands.iter().enumerate()
                                {
                                    p.set_band_gain(i, db);
                                }
                                p.apply_audio(&settings.audio);
                            }
                        }
                        // The item's page url keys the persistent
                        // stream cache — shared by playback and the
                        // waveform worker, surviving restarts.
                        let key = path.to_string_lossy().into_owned();
                        if let Some(p) = player {
                            let _ = p.open_stream(&stream, &key);
                        }
                        // Stream waveform: read-only consumer of that
                        // same cache — no second download; buckets fill
                        // at disk speed for fetched bytes, then at the
                        // live download rate.
                        if let Some(c) = waveform_cancel.take() {
                            c.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        let cancel = std::sync::Arc::new(
                            std::sync::atomic::AtomicBool::new(false),
                        );
                        *waveform_rx = Some(audio::waveform::spawn_stream(
                            stream,
                            key,
                            cancel.clone(),
                        ));
                        *waveform_cancel = Some(cancel);
                    } else {
                        *waveform_loading = false;
                    }
                    *resolve_rx = None;
                }
                Ok((_, Err(_)))
                | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    *waveform_loading = false;
                    *resolve_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // Playlist expansion landed — add every member (deduped) and
        // fetch their metadata (Lacrima's AddPlaylistAsync batch).
        if let Some(rx) = playlist_rx.as_ref() {
            match rx.try_recv() {
                Ok(urls) => {
                    url_prompt.playlist_busy = false;
                    let mut added = Vec::new();
                    for u in urls {
                        if carousel.add_url(&u) {
                            added.push(u);
                        }
                    }
                    if !added.is_empty() {
                        persist_url_items(carousel, albums, settings);
                        let title = carousel
                            .display_for(std::path::Path::new(&added[0]))
                            .2;
                        let paths: Vec<std::path::PathBuf> =
                            added.iter().map(std::path::PathBuf::from).collect();
                        *url_info_rx =
                            Some(library::spawn_url_info(paths, title));
                        ui.ctx().request_repaint();
                    }
                    *playlist_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    url_prompt.playlist_busy = false;
                    *playlist_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // Metadata for loaded items lands per-item from the worker
        // pool — patch each label as it arrives (and swap the guessed
        // thumbnail for the real one on url items). The receiver is
        // dropped only when every worker finished.
        let mut infos_done = false;
        let mut infos_landed = false;
        if let Some(rx) = url_info_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(infos) => {
                        for (p, info) in infos {
                            carousel.apply_info(&p, &info);
                        }
                        infos_landed = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        infos_done = true;
                        break;
                    }
                }
            }
        }
        if infos_done {
            *url_info_rx = None;
        }
        if infos_landed {
            // Items' covers upgraded to the resolved thumbs — mirror
            // them into the loaded album tile's web fan so the tile
            // tracks item updates live.
            if let Some(key) = carousel.album_key().map(|k| k.to_string()) {
                if let Some(a) = albums
                    .iter_mut()
                    .find(|a| a.dir.to_string_lossy() == key)
                {
                    let covers = carousel.url_covers();
                    let n = covers.len();
                    a.sync_url_covers(covers, n);
                }
            }
            ui.ctx().request_repaint();
        }
        // The opened album's top cover decoded on a worker — apply the
        // spectrum gradient + shadertoy palette, but only if that
        // album is still the loaded one.
        if let Some(rx) = album_cover_rx.as_ref() {
            match rx.try_recv() {
                Ok((dir, img)) => {
                    if carousel.album_key() == Some(dir.as_str()) {
                        *spectrum_colors = img
                            .as_ref()
                            .map(|i| spectrum_bar::dominant_colors(i, 3));
                        if let (Some(toy), Some(img)) =
                            (shadertoy.as_mut(), img.as_ref())
                        {
                            toy.set_cover_image(img);
                            if let Some(c) = spectrum_colors.as_deref() {
                                toy.set_palette(c);
                            }
                        }
                    }
                    *album_cover_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    *album_cover_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // The url item's remote cover decoded on a worker — apply it
        // to the vinyl label + spectrum/shadertoy palette, but only
        // if it's still the playing item.
        if let Some(rx) = playing_cover_rx.as_ref() {
            if let Ok((path, img)) = rx.try_recv() {
                if playing_track.as_ref() == Some(&path) {
                    if let Some(img) = img {
                        *spectrum_colors =
                            Some(spectrum_bar::dominant_colors(&img, 3));
                        if let Some(toy) = shadertoy.as_mut() {
                            toy.set_cover_image(&img);
                            if let Some(c) = spectrum_colors.as_deref() {
                                toy.set_palette(c);
                            }
                        }
                        *playing_cover = Some(ui.ctx().load_texture(
                            "player-cover",
                            img,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
                *playing_cover_rx = None;
            }
        }
        // Pump the spectrum tap once per frame (Lacrima's analyzer also
        // runs regardless of the visible view). Bars settling after a
        // pause need frames to decay.
        if spectrum.update(player.as_mut(), dt, &settings.spectrum)
            && nav.alpha(View::Music) > 0.0
        {
            ui.ctx().request_repaint();
        }
        // Feed the shadertoy's iChannel0 spectrum texture + iMouse, and
        // keep the GPU selection in sync with the persisted name (the
        // UI-tab ComboBox writes settings, this applies it live).
        if let Some(toy) = shadertoy {
            toy.update_spectrum(spectrum.shader_bins());
            if toy.current() != settings.effects.shadertoy {
                toy.select(&settings.effects.shadertoy);
            }
            let ctx = ui.ctx();
            let ppp = ctx.pixels_per_point();
            let h_px = frame_rect.height() * ppp;
            let pos = ctx.input(|i| i.pointer.latest_pos());
            let org = ctx.input(|i| i.pointer.press_origin());
            let down = ctx.input(|i| i.pointer.primary_down());
            let (mx, my) = pos
                .map(|p| (p.x * ppp, h_px - p.y * ppp))
                .unwrap_or((0.0, 0.0));
            let (cx, cy) = org.map(|p| (p.x * ppp, h_px - p.y * ppp)).unwrap_or((mx, my));
            toy.set_mouse([mx, my, if down { cx } else { -cx }, cy]);
            // u_disc occluder — the player widget's disc in GL coords
            // (ShaderDiscOccluderSync); inactive when the widget is hidden
            // or off the Main view.
            let d = if player_widget.visible
                && player_widget.rect.is_positive()
                && nav.alpha(View::Main) > 0.001
            {
                Some(widgets::disc_layout(player_widget.rect))
            } else {
                None
            };
            if let Some(d) = d {
                toy.set_disc(d.center.x * ppp, h_px - d.center.y * ppp, d.ring_radius * ppp, true);
            } else {
                toy.set_disc(0.0, 0.0, 0.0, false);
            }
        }
        // Track tag metadata landed — upgrade the cover items' labels.
        let infos = track_info_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        if let Some(infos) = infos {
            carousel.set_info(infos);
            *track_info_rx = None;
        }
        let back_icon = icons.back.id();
        let settings_icon = icons.settings.id();
        let settings_active = *settings_open;
        let animate_title = settings.effects.animate_title;
        let interactive = matches!(nav, Nav::Settled(_));
        // While the settings overlay is open (or sliding) nothing
        // behind it accepts input — including the carousel's raw
        // pointer paths that bypass egui's widget hit routing.
        let overlay_slide = ui.ctx().animate_bool_with_time(
            egui::Id::new("settings-overlay"),
            *settings_open,
            0.3,
        );
        let view_interactive = interactive
            && overlay_slide < 0.001
            && !tag_editor.is_open()
            && !url_prompt.is_open()
            && !fs_exit.open
            && !del_prompt.open;
        // The scrollbar grab zone owns the window's bottom edge in the
        // Music view: suppress the resize grip over the same rect so
        // presses reach the scrollbar instead of starting a resize.
        let resize_exclude = if (nav.target() == View::Music && transport.album_list_open)
            || (nav.target() == View::Emulation && emu.list_open)
        {
            let row = album_row_rect(content, s);
            Some(Rect::from_min_max(
                album_row::AlbumRow::scroll_zone(row, s).min,
                content.max,
            ))
        } else {
            None
        };
        lacrima_window_frame(
            ui,
            "AES Rustified",
            logo,
            elapsed,
            back_icon,
            back_alpha,
            &mut back_clicked,
            settings_icon,
            settings_active,
            &mut gear_clicked,
            animate_title,
            s,
            resize_exclude,
            &mut fs_exit_req,
            |ui| {
                // Right-click menu like Lacrima's view context menu —
                // suppressed over the albums strip, which has its own.
                let resp = ui.interact(content, egui::Id::new("content-area"), Sense::click());
                let over_album_row = nav.target() == View::Music
                    && ui
                        .ctx()
                        .input(|i| i.pointer.latest_pos())
                        .map(|p| album_row_rect(content, s).contains(p))
                        .unwrap_or(false);
                if !tag_editor.is_open() && !url_prompt.is_open() && !fs_exit.open {
                    resp.context_menu(|ui| {
                        if !over_album_row {
                            widget_menu(
                                ui,
                                &mut clock.frame,
                                &mut player_widget.frame,
                                widget_edit,
                                content,
                                frame_rect,
                                s,
                            );
                        }
                    });
                }

                let mut ended = false;
                let mut row_action = album_row::Action::None;
                let mut seek_secs = None;
                let mut play_path = None;
                let mut edit_path = None;
                let mut prompt_kind = None;
                let mut delete_path = None;
                let mut clear_album = false;
                let mut export_album = false;
                // A delete confirmed last frame re-injects as the
                // original action — the `confirmed` flags make the
                // handlers run instead of re-asking the prompt.
                let mut items_confirmed = false;
                let mut albums_confirmed = false;
                match del_prompt.confirmed.take() {
                    Some(PendingDelete::Items(p)) => {
                        delete_path = Some(p);
                        items_confirmed = true;
                    }
                    Some(PendingDelete::Albums(d)) => {
                        row_action = album_row::Action::DeleteAlbum(d);
                        albums_confirmed = true;
                    }
                    Some(PendingDelete::Roms(p)) => {
                        emu.remove_games(&mut settings.emulation, &p);
                    }
                    None => {}
                }
                // ROM-item removals from the card menu wait on the
                // same confirmation dialog.
                if let Some(paths) = emu.pending_delete.take() {
                    del_prompt.request(PendingDelete::Roms(paths), ui.ctx());
                }
                // EQ band gains mutate inside music_view — snapshot for
                // the dirty check since fx_before is taken after it.
                let eq_before = settings.equalizer.bands;
                ui.scope_builder(UiBuilder::new().max_rect(content), |ui| {
                    let a = nav.alpha(View::Main);
                    if a > 0.0 {
                        ui.set_opacity(a);
                        if let Some(i) = main_menu(
                            ui,
                            icons,
                            menu_scales,
                            view_interactive,
                            *settings_open,
                            s,
                        ) {
                            menu_activate(
                                i,
                                menu_entries(icons).len(),
                                settings_open,
                                nav,
                            );
                        }
                        let cout = clock.ui(
                            ui,
                            content,
                            frame_rect,
                            *widget_edit && view_interactive,
                            elapsed,
                            s,
                        );
                        ended = cout.ended;
                        let pout = player_widget.ui(
                            ui,
                            content,
                            frame_rect,
                            *widget_edit && view_interactive,
                            view_interactive,
                            elapsed,
                            s,
                            playing_cover.as_ref(),
                            player
                                .as_ref()
                                .map(|p| p.position().as_secs_f64())
                                .unwrap_or(0.0),
                            player
                                .as_ref()
                                .map(|p| p.duration().as_secs_f64())
                                .unwrap_or(0.0),
                            player
                                .as_ref()
                                .map(|p| p.state() == audio::PlayerState::Playing)
                                .unwrap_or(false),
                        );
                        ended = ended || pout.ended;
                        // The ring is a circular seek bar; the arm click
                        // toggles play/pause (SetPositionCommand /
                        // TogglePlayCommand).
                        if let Some(t) = pout.seek {
                            if let Some(p) = player.as_ref() {
                                p.seek(t);
                            }
                        }
                        if pout.toggle_play {
                            if let Some(p) = player.as_ref() {
                                p.toggle();
                            }
                        }
                        for r in [cout.menu_resp, pout.menu_resp].into_iter().flatten() {
                            r.context_menu(|ui| {
                                widget_menu(
                                    ui,
                                    &mut clock.frame,
                                    &mut player_widget.frame,
                                    widget_edit,
                                    content,
                                    frame_rect,
                                    s,
                                )
                            });
                        }
                    }
                    for v in View::SUB_VIEWS {
                        let a = nav.alpha(v);
                        if a > 0.0 {
                            ui.set_opacity(a);
                            match v {
                                View::Music => {
                                    // The playing item's card spins
                                    // while its stream url resolves or
                                    // the decoder preroll fills —
                                    // cleared the moment playback runs.
                                    let stream_loading = playing_track
                                        .as_ref()
                                        .is_some_and(|t| library::is_url(t))
                                        && (resolve_rx.is_some()
                                            || player.as_ref().is_some_and(|p| {
                                                p.buffering()
                                                    && p.state()
                                                        == audio::PlayerState::Playing
                                            }));
                                    // Shift held = reorder armed —
                                    // the owning list lifts its
                                    // selection to signal the arrows
                                    // now move it (same routing as
                                    // the Shift+arrow handler).
                                    let shift_down = view_interactive
                                        && !ui.ctx().text_edit_focused()
                                        && ui.ctx().input(|i| i.modifiers.shift);
                                    let cards_armed = shift_down
                                        && (*sel_owner == SelOwner::Cards
                                            || (*sel_owner == SelOwner::None
                                                && carousel.has_items()));
                                    let row_armed = shift_down
                                        && (*sel_owner == SelOwner::Albums
                                            || (*sel_owner == SelOwner::None
                                                && !carousel.has_items()));
                                    let (ra, sk, play, ed, pr, del, cl, ex) = music_view(
                                        ui,
                                        content,
                                        a,
                                        view_interactive,
                                        albums,
                                        album_row,
                                        carousel,
                                        player.as_ref(),
                                        playing_track.as_deref(),
                                        stream_loading,
                                        waveform_shown,
                                        *waveform_loading,
                                        waveform_bar,
                                        transport,
                                        spectrum,
                                        &settings.spectrum,
                                        &settings.carousel,
                                        &mut settings.equalizer,
                                        spectrum_colors.as_deref(),
                                        Color32::from_rgb(
                                            settings.audio.volume_knob_color[0],
                                            settings.audio.volume_knob_color[1],
                                            settings.audio.volume_knob_color[2],
                                        ),
                                        search,
                                        album_search,
                                        radius,
                                        s,
                                        album_rx.is_some(),
                                        row_armed,
                                        cards_armed,
                                    );
                                    row_action = ra;
                                    seek_secs = sk;
                                    play_path = play;
                                    edit_path = ed;
                                    prompt_kind = pr;
                                    delete_path = del;
                                    clear_album = cl;
                                    export_album = ex;
                                }
                                View::Emulation => {
                                    // The layout toggle shares the
                                    // global CoverLayoutMode — the same
                                    // setting the music band cycles.
                                    emulation::view(
                                        ui,
                                        content,
                                        a,
                                        view_interactive,
                                        emu,
                                        &mut settings.emulation,
                                        &settings.carousel,
                                        &mut transport.cover_layout,
                                        radius,
                                        s,
                                    );
                                }
                                _ => placeholder_view(ui, content, v.name(), s),
                            }
                        }
                    }
                });
                // A landed drag reorder persists the dir order so the
                // next scan (and next launch) restores it — the
                // manifest cache too, or next launch's seeded order
                // would briefly show the pre-drag layout.
                if album_row.take_order_dirty() {
                    settings.library.album_order = albums
                        .iter()
                        .map(|a| a.dir.to_string_lossy().into_owned())
                        .collect();
                    settings.library.cached_albums =
                        settings.library.album_order.clone();
                    settings::save(settings);
                }
                // A landed item reorder: the new URL order persists
                // and the tile's cover fan re-syncs to the new first
                // item (file items reorder in view only — their order
                // comes from the folder scan).
                if carousel.take_reorder_dirty() {
                    persist_url_items(carousel, albums, settings);
                }
                // Selection ownership follows whichever view the user
                // last selected in — a stale highlight in the other
                // view never fires the Delete key.
                if album_row.take_sel_touched() {
                    *sel_owner = SelOwner::Albums;
                }
                if carousel.take_sel_touched() {
                    *sel_owner = SelOwner::Cards;
                }
                // Emulation game-list edits (Add Roms / Scan Folder /
                // Remove / Clear, Xenia path) persist immediately.
                if emu.take_settings_dirty() {
                    settings::save(settings);
                }
                // Enter on the Emulation view launches the selected
                // game when no session is running — with a game in
                // the background it switches back to the capture
                // viewport instead (one emulator at a time).
                if view_interactive
                    && nav.target() == View::Emulation
                    && (emu.session.is_none() || emu.viewport_dismissed)
                    && emu.active.is_some()
                    && !emu.editor.is_open()
                    && !emu.patches.is_open()
                    && !emu.cfg_ov.is_open()
                    && !emu.render_open
                    && !ui.ctx().text_edit_focused()
                    && ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    })
                {
                    if emu.session.is_some() {
                        emu.viewport_dismissed = false;
                    } else if let Some(game) = emu.cards.primary_path() {
                        emu.launch(&settings.emulation, &game);
                    }
                }
                // The Emulation view owns the keys while it's up —
                // the ROM cards navigate/select exactly like the
                // audio list (arrows, Ctrl+A, Delete → confirm). With
                // a session running they only work while the capture
                // viewport is dismissed (the list is the visible UI).
                if view_interactive
                    && nav.target() == View::Emulation
                    && (emu.session.is_none() || emu.viewport_dismissed)
                    && emu.active.is_some()
                    && !emu.editor.is_open()
                    && !emu.patches.is_open()
                    && !emu.cfg_ov.is_open()
                    && !emu.render_open
                    && !ui.ctx().text_edit_focused()
                {
                    let (mut dx, mut dy) = (0i32, 0i32);
                    ui.ctx().input_mut(|i| {
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                            dx -= 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                            dx += 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                            dy -= 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                            dy += 1;
                        }
                    });
                    if dx != 0 || dy != 0 {
                        emu.cards.arrow(&emu.search.text, dx, dy);
                    }
                    if ui
                        .ctx()
                        .input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A))
                    {
                        emu.cards.select_all(&emu.search.text);
                    }
                    if ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
                    }) {
                        let paths = emu.cards.selected_paths();
                        if !paths.is_empty() {
                            emu.pending_delete = Some(paths);
                        }
                    }
                }
                // egui's own focus navigation (begin_pass reads raw Tab/
                // arrow events before our consume_key runs) hands focus
                // to the first focusable widget — a focused button then
                // blocks every key via wants_keyboard_input and eats
                // Enter as a fake click. Our nav owns the keys: drop
                // stray widget focus each frame (a real TextEdit keeps
                // its focus — it genuinely needs them).
                if view_interactive
                    && !ui.ctx().text_edit_focused()
                    && !search.wants_focus()
                    && !album_search.wants_focus()
                {
                    ui.ctx().memory_mut(|m| m.stop_text_input());
                }
                // Delete: route to the owning view's selection.
                // Text fields eat the key first (text_edit_focused)
                // and overlays gate via view_interactive. The
                // Emulation view owns its keys separately below.
                if view_interactive
                    && nav.target() != View::Emulation
                    && !ui.ctx().text_edit_focused()
                    && ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
                    })
                {
                    match *sel_owner {
                        SelOwner::Albums => {
                            let dirs = album_row.selected_dirs(albums);
                            if !dirs.is_empty() {
                                row_action =
                                    album_row::Action::DeleteAlbum(dirs);
                            }
                        }
                        SelOwner::Cards => {
                            let paths = carousel.selected_paths();
                            delete_path = (!paths.is_empty()).then_some(paths);
                        }
                        SelOwner::None => {}
                    }
                }
                // Ctrl+A — select everything in the owning view (same
                // owner routing as Delete: the list after a list
                // gesture, the cards after a card gesture).
                if view_interactive
                    && nav.target() != View::Emulation
                    && !ui.ctx().text_edit_focused()
                    && ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::COMMAND, egui::Key::A)
                    })
                {
                    match *sel_owner {
                        SelOwner::Albums => album_row.select_all(albums),
                        SelOwner::Cards => carousel.select_all(&search.text),
                        SelOwner::None => {}
                    }
                }
                // Arrow-key navigation — same owner routing. With no
                // owner yet, a loaded album's cards win (the cover
                // view is the primary surface), else the album strip.
                // Emulation routes its own keys below.
                if view_interactive
                    && nav.target() != View::Emulation
                    && !ui.ctx().text_edit_focused()
                {
                    let (mut dx, mut dy) = (0i32, 0i32);
                    let (mut mdx, mut mdy) = (0i32, 0i32);
                    ui.ctx().input_mut(|i| {
                        if i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowLeft) {
                            mdx -= 1;
                        }
                        if i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowRight) {
                            mdx += 1;
                        }
                        if i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp) {
                            mdy -= 1;
                        }
                        if i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown) {
                            mdy += 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                            dx -= 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                            dx += 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                            dy -= 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                            dy += 1;
                        }
                    });
                    // Shift+arrows reorder; plain arrows navigate —
                    // same owner routing either way.
                    if mdx != 0 || mdy != 0 {
                        match *sel_owner {
                            SelOwner::Albums => {
                                album_row.move_selection(albums, mdx + mdy)
                            }
                            SelOwner::Cards => {
                                carousel.move_selection(&search.text, mdx, mdy)
                            }
                            SelOwner::None => {
                                if carousel.has_items() {
                                    carousel.move_selection(&search.text, mdx, mdy)
                                } else {
                                    album_row.move_selection(albums, mdx + mdy)
                                }
                            }
                        }
                    } else if dx != 0 || dy != 0 {
                        match *sel_owner {
                            SelOwner::Albums => album_row.arrow(albums, dx + dy),
                            SelOwner::Cards => {
                                carousel.arrow(&search.text, dx, dy)
                            }
                            SelOwner::None => {
                                if carousel.has_items() {
                                    carousel.arrow(&search.text, dx, dy)
                                } else {
                                    album_row.arrow(albums, dx + dy)
                                }
                            }
                        }
                    }
                }
                // Tab — cycle focus between the album strip and the
                // loaded album's items (Lacrima's focus cycle). The
                // target view reveals its current selection; with no
                // cards loaded the strip keeps focus.
                if view_interactive
                    && nav.target() != View::Emulation
                    && !ui.ctx().text_edit_focused()
                    && ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                    })
                {
                    let next = match *sel_owner {
                        SelOwner::Albums | SelOwner::None => SelOwner::Cards,
                        SelOwner::Cards => SelOwner::Albums,
                    };
                    *sel_owner = if next == SelOwner::Cards && !carousel.has_items() {
                        SelOwner::Albums
                    } else {
                        next
                    };
                    match *sel_owner {
                        SelOwner::Albums => album_row.focus(albums),
                        SelOwner::Cards => carousel.focus(&search.text),
                        SelOwner::None => {}
                    }
                }
                // Enter — activates the owning view's selection like a
                // double-click: plays the card, opens the album.
                if view_interactive
                    && nav.target() != View::Emulation
                    && !ui.ctx().text_edit_focused()
                    && ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    })
                {
                    match *sel_owner {
                        SelOwner::Albums => {
                            if let Some(i) = album_row.selected_index() {
                                row_action = album_row::Action::Opened(i);
                            }
                        }
                        SelOwner::Cards => {
                            if let Some(p) = carousel.primary_path() {
                                play_path = Some(p);
                            }
                        }
                        SelOwner::None => {}
                    }
                }
                // Ctrl+F — focus the search box for whichever list
                // owns selection: the band's album filter for the
                // strip, the top-right item search for the cards.
                // The Emulation view's item pill takes it there too.
                if view_interactive
                    && ui.ctx().input_mut(|i| {
                        i.consume_key(egui::Modifiers::CTRL, egui::Key::F)
                    })
                {
                    match nav.target() {
                        View::Music => match *sel_owner {
                            SelOwner::Albums => album_search.open(),
                            SelOwner::Cards => search.open(),
                            SelOwner::None => {
                                if carousel.has_items() {
                                    search.open()
                                } else {
                                    album_search.open()
                                }
                            }
                        },
                        View::Emulation => emu.search.open(),
                        _ => {}
                    }
                }
                // Esc — the exit-fullscreen prompt closes first (and
                // stays fullscreen); then an open search box; then a
                // focused text field owns it. In fullscreen it opens
                // the Switch-style exit confirmation; otherwise Esc
                // and Backspace are the back button. The overlays'
                // own Esc handling runs while they're open —
                // view_interactive gates this path then.
                if view_interactive || fs_exit.open || del_prompt.open {
                    let esc = ui
                        .ctx()
                        .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                    if esc {
                        if fs_exit.open {
                            fs_exit.open = false;
                        } else if del_prompt.open {
                            del_prompt.dismiss();
                        } else if search.is_open() {
                            search.close(ui.ctx());
                        } else if album_search.is_open() {
                            album_search.close(ui.ctx());
                        } else if emu.search.is_open() {
                            emu.search.close(ui.ctx());
                        } else if emu.render_open {
                            // The capture render-options panel closes
                            // like the other overlays.
                            emu.render_open = false;
                        } else if nav.target() == View::Emulation
                            && emu.session.is_some()
                            && emu.game_fs
                        {
                            // Esc backs out of game-fullscreen first —
                            // the session keeps running.
                            emu.game_fs = false;
                        } else if nav.target() == View::Emulation && emu.session.is_some() {
                            // Esc while a game runs closes the
                            // emulator session (graceful WM_CLOSE,
                            // TerminateProcess fallback).
                            emu.stop_session();
                        } else if !ui.ctx().text_edit_focused() {
                            if ui
                                .ctx()
                                .input(|i| i.viewport().fullscreen.unwrap_or(false))
                            {
                                fs_exit.begin(ui.ctx());
                            } else {
                                esc_back = true;
                            }
                        }
                    }
                    if !fs_exit.open
                        && !del_prompt.open
                        && !ui.ctx().text_edit_focused()
                        && ui.ctx().input_mut(|i| {
                            i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)
                        })
                    {
                        esc_back = true;
                    }
                }
                // F12 toggles fullscreen — entering is instant, leaving
                // goes through the same exit confirmation as Esc.
                if view_interactive
                    && ui
                        .ctx()
                        .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F12))
                {
                    if ui
                        .ctx()
                        .input(|i| i.viewport().fullscreen.unwrap_or(false))
                    {
                        fs_exit.begin(ui.ctx());
                    } else {
                        ui.ctx()
                            .send_viewport_cmd(ViewportCommand::Fullscreen(true));
                    }
                }
                // Ctrl+arrows — Up/Down nudge the volume, Left/Right
                // skip 5s. Text fields keep their word-jump/caret keys.
                if view_interactive && !ui.ctx().text_edit_focused() {
                    let (vol_up, vol_dn, skip_b, skip_f) = ui.ctx().input_mut(|i| {
                        (
                            i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowUp),
                            i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowDown),
                            i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowLeft),
                            i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowRight),
                        )
                    });
                    if (vol_up || vol_dn || skip_b || skip_f) && player.is_some() {
                        if let Some(p) = player.as_ref() {
                            if vol_up || vol_dn {
                                p.set_volume(
                                    (p.volume() + if vol_up { 0.05 } else { -0.05 })
                                        .clamp(0.0, 1.0),
                                );
                            }
                            if skip_b || skip_f {
                                let pos = p.position().as_secs_f64();
                                p.seek((pos + if skip_f { 5.0 } else { -5.0 }).max(0.0));
                            }
                        }
                    }
                }
                // Number keys activate the main-menu entries in
                // order — the binding follows `menu_entries`, so a
                // new entry automatically takes the next number.
                // Only on the Main view where the menu lives.
                if view_interactive
                    && !ui.ctx().text_edit_focused()
                    && nav.target() == View::Main
                {
                    let menu_n = menu_entries(icons).len();
                    for n in 1..=menu_n.min(9) {
                        let key = match n {
                            1 => egui::Key::Num1,
                            2 => egui::Key::Num2,
                            3 => egui::Key::Num3,
                            4 => egui::Key::Num4,
                            5 => egui::Key::Num5,
                            6 => egui::Key::Num6,
                            7 => egui::Key::Num7,
                            8 => egui::Key::Num8,
                            _ => egui::Key::Num9,
                        };
                        if ui
                            .ctx()
                            .input_mut(|i| i.consume_key(egui::Modifiers::NONE, key))
                        {
                            menu_activate(n - 1, menu_n, settings_open, nav);
                            break;
                        }
                    }
                }
                // Alt is a modifier, not a Key event — a clean tap
                // (press + release with no other key and the window
                // focused) cycles the cover layout like the band's
                // layout button. Alt+Tab presses other keys and/or
                // drops focus, so it can never fire this.
                {
                    // Track the Alt key events themselves — AltGr
                    // arrives as ControlLeft+AltRight and Windows
                    // modifier state can't distinguish it from a
                    // real Ctrl+Alt, so edges on the Alt KEYS are
                    // the reliable signal for both Alt keys.
                    let is_mod = |k: egui::Key| {
                        matches!(
                            k,
                            egui::Key::ShiftLeft
                                | egui::Key::ShiftRight
                                | egui::Key::ControlLeft
                                | egui::Key::ControlRight
                                | egui::Key::AltLeft
                                | egui::Key::AltRight
                                | egui::Key::SuperLeft
                                | egui::Key::SuperRight
                        )
                    };
                    let (mut pressed, mut released) = (false, false);
                    let (other_key, focused) = ui.ctx().input(|i| {
                        for ev in &i.events {
                            if let egui::Event::Key { key, pressed: p, .. } = ev {
                                match (key, p) {
                                    (egui::Key::AltLeft | egui::Key::AltRight, true) => {
                                        pressed = true
                                    }
                                    (egui::Key::AltLeft | egui::Key::AltRight, false) => {
                                        released = true
                                    }
                                    _ => {}
                                }
                            }
                        }
                        (
                            i.keys_down.iter().any(|k| !is_mod(*k)),
                            i.focused,
                        )
                    });
                    if pressed {
                        *alt_down = true;
                        *alt_clean = !other_key;
                    }
                    if *alt_down && other_key {
                        *alt_clean = false;
                    }
                    if released {
                        if *alt_down
                            && *alt_clean
                            && focused
                            && view_interactive
                            && !ui.ctx().text_edit_focused()
                            && nav.target() == View::Music
                        {
                            transport.cover_layout =
                                (transport.cover_layout + 1) % 3;
                        }
                        *alt_down = false;
                        *alt_clean = false;
                    }
                }
                match row_action {
                    album_row::Action::AddFolder => {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("Add music folder")
                            .pick_folder()
                        {
                            settings.library.music_roots.push(dir);
                            settings::save(settings);
                            album_row.reset();
                            scan_seen.clear();
                            *album_rx =
                                Some(library::spawn_scan(&settings.library.music_roots));
                        }
                    }
                    album_row::Action::Rescan => {
                        album_row.reset();
                        scan_seen.clear();
                        *album_rx = Some(library::spawn_scan(&settings.library.music_roots));
                    }
                    album_row::Action::DeleteAlbum(dirs) if !albums_confirmed => {
                        del_prompt.request(PendingDelete::Albums(dirs), ui.ctx());
                    }
                    album_row::Action::DeleteAlbum(dirs) => {
                        // Lacrima's DeleteFolder per album (context
                        // menu: one dir; Delete key: the selection):
                        // remove from the list (files stay on disk —
                        // the dir is recorded so scans don't resurrect
                        // it), stop playback if a child was playing,
                        // and clear the cover view if it was loaded.
                        for dir in dirs {
                            if let Some(i) =
                                albums.iter().position(|a| {
                                    a.dir.to_string_lossy() == dir
                                })
                            {
                                if carousel.album_key() == Some(dir.as_str()) {
                                    if let Some(cur) = playing_track.as_ref() {
                                        if carousel.index_of(cur).is_some() {
                                            if let Some(p) = player {
                                                p.stop();
                                            }
                                            *playing_track = None;
                                            *playing_cover = None;
                                            *waveform_loading = false;
                                        }
                                    }
                                    carousel.close();
                                }
                                albums.remove(i);
                                album_row.removed(i);
                                album_row.deselect_dir(&dir);
                                let lib = &mut settings.library;
                                // Imported albums aren't scan results —
                                // deleting removes the import entry itself
                                // rather than recording a scan exclusion.
                                if library::is_virtual_dir(std::path::Path::new(&dir)) {
                                    lib.virtual_albums.retain(|a| a.dir != dir);
                                } else if !lib.excluded_albums.contains(&dir) {
                                    lib.excluded_albums.push(dir.clone());
                                }
                                // The album's url items are gone with it —
                                // their stream/metadata/cover caches too.
                                if let Some(urls) = lib.url_items.remove(&dir) {
                                    for u in urls {
                                        evict_url_caches(&u);
                                    }
                                }
                                lib.album_order.retain(|d| d != &dir);
                                lib.cached_albums.retain(|d| d != &dir);
                                lib.album_titles.remove(&dir);
                                lib.excluded_tracks.remove(&dir);
                            }
                        }
                        settings::save(settings);
                    }
                    album_row::Action::Export(dir) => {
                        // The tile menu's export — same document as the
                        // cover-view export; labels come from the
                        // loaded carousel when this album is open,
                        // persisted urls otherwise.
                        if let Some(i) =
                            albums.iter().position(|a| {
                                a.dir.to_string_lossy() == dir
                            })
                        {
                            let title = albums[i].title.clone();
                            let items = if carousel.album_key()
                                == Some(dir.as_str())
                            {
                                carousel.export_items()
                            } else {
                                settings
                                    .library
                                    .url_items
                                    .get(&dir)
                                    .cloned()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|url| apl::Item {
                                        url,
                                        ..Default::default()
                                    })
                                    .collect()
                            };
                            if !items.is_empty() {
                                let file_name = {
                                    let n =
                                        apl::sanitize_file_name(&title);
                                    if n.is_empty() {
                                        "playlist".to_string()
                                    } else {
                                        n
                                    }
                                };
                                if let Some(file) = rfd::FileDialog::new()
                                    .set_title("Export Online Playlist")
                                    .set_file_name(format!(
                                        "{file_name}.apl"
                                    ))
                                    .add_filter(
                                        "AES Online Playlist",
                                        &["apl"],
                                    )
                                    .save_file()
                                {
                                    if let Err(e) =
                                        apl::write(&file, &title, items)
                                    {
                                        eprintln!(
                                            "apl export failed: {e}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    album_row::Action::Import => {
                        // Lacrima's ImportPlaylistAsync: pick an .apl,
                        // add a uniquely-titled album of its URL items
                        // (deduped like IsMediaDuplicate), then open it.
                        if let Some(file) = rfd::FileDialog::new()
                            .set_title("Import Online Playlist")
                            .add_filter("AES Online Playlist", &["apl"])
                            .pick_file()
                        {
                            if let Ok(doc) = apl::read(&file) {
                                let base = doc
                                    .name
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|n| !n.is_empty())
                                    .unwrap_or("Imported Playlist");
                                let mut title = base.to_string();
                                let mut n = 2;
                                while albums.iter().any(|a| a.title == title) {
                                    title = format!("{base} ({n})");
                                    n += 1;
                                }
                                let mut urls: Vec<String> = Vec::new();
                                for it in &doc.items {
                                    let u = it.url.trim();
                                    if u.is_empty()
                                        || !(u.starts_with("http://")
                                            || u.starts_with("https://"))
                                    {
                                        continue;
                                    }
                                    let id = ytdlp::extract_video_id(u);
                                    let dup = urls.iter().any(|e| {
                                        e == u
                                            || (id.is_some()
                                                && ytdlp::extract_video_id(e) == id)
                                    });
                                    if !dup {
                                        urls.push(u.to_string());
                                    }
                                }
                                if !urls.is_empty() {
                                    let dir = format!(
                                        "{}{}",
                                        library::VIRTUAL_SCHEME,
                                        title
                                    );
                                    albums.push(library::Album::virtual_album(
                                        &dir,
                                        title.clone(),
                                        &urls,
                                    ));
                                    let lib = &mut settings.library;
                                    lib.virtual_albums.push(
                                        settings::VirtualAlbum {
                                            dir: dir.clone(),
                                            title,
                                        },
                                    );
                                    lib.url_items
                                        .insert(dir, urls.clone());
                                    settings::save(settings);
                                    // Open it (the album tile's web
                                    // covers load on their own worker
                                    // threads — no synchronous fetch).
                                    if let Some(album) = albums.last() {
                                        let album_title =
                                            album.title.clone();
                                        let (tracks, t) =
                                            carousel.open(album, &urls);
                                        // The .apl carries display
                                        // labels — seed them now;
                                        // workers still re-resolve
                                        // fresh metadata + thumbnails.
                                        for u in &urls {
                                            let Some(it) = doc
                                                .items
                                                .iter()
                                                .find(|i| {
                                                    i.url.trim() == u
                                                })
                                            else {
                                                continue;
                                            };
                                            let title = it
                                                .title
                                                .as_deref()
                                                .map(str::trim)
                                                .filter(|s| {
                                                    !s.is_empty()
                                                });
                                            let artist = it
                                                .artist
                                                .clone()
                                                .unwrap_or_default();
                                            if title.is_none()
                                                && artist
                                                    .trim()
                                                    .is_empty()
                                            {
                                                continue;
                                            }
                                            // Persist into the
                                            // metadata cache too — the
                                            // .apl's labels are the
                                            // imported source of truth,
                                            // so reloads need no fetch.
                                            meta_cache::save(
                                                u,
                                                &meta_cache::CachedMeta {
                                                    title: title
                                                        .unwrap_or(u)
                                                        .to_string(),
                                                    artist: artist
                                                        .clone(),
                                                    thumb: None,
                                                    ..Default::default()
                                                },
                                            );
                                            carousel.apply_info(
                                                std::path::Path::new(
                                                    u.as_str(),
                                                ),
                                                &library::TrackInfo {
                                                    title: title
                                                        .unwrap_or(u)
                                                        .to_string(),
                                                    artist,
                                                    album: it
                                                        .album
                                                        .clone()
                                                        .unwrap_or_else(
                                                            || {
                                                                album_title
                                                                    .clone()
                                                            },
                                                        ),
                                                    number: None,
                                                    cover_url: None,
                                                },
                                            );
                                        }
                                        *url_info_rx = Some(
                                            library::spawn_url_info(
                                                tracks, t,
                                            ),
                                        );
                                    }
                                    ui.ctx().request_repaint();
                                }
                            }
                        }
                    }
                    album_row::Action::Create => {
                        // Empty virtual album appended at the end; the
                        // row snaps it into view and opens the inline
                        // title editor (Lacrima's AddUserAlbum). The
                        // `apl://user-{id}` key is title-independent —
                        // renames never break its UrlItems/order links.
                        let mut title = "New Album".to_string();
                        let mut n = 2;
                        while albums.iter().any(|a| a.title == title) {
                            title = format!("New Album ({n})");
                            n += 1;
                        }
                        let dir = format!(
                            "{}user-{}",
                            library::VIRTUAL_SCHEME,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        );
                        albums.push(library::Album::virtual_album(
                            &dir,
                            title.clone(),
                            &[],
                        ));
                        let lib = &mut settings.library;
                        lib.virtual_albums.push(settings::VirtualAlbum {
                            dir: dir.clone(),
                            title: title.clone(),
                        });
                        lib.url_items.insert(dir.clone(), Vec::new());
                        settings::save(settings);
                        album_row.edit_album(&dir, &title);
                    }
                    album_row::Action::Rename(dir, title) => {
                        // A display-title override keyed by dir — the
                        // folder name on disk is never touched and the
                        // album's url_items/order/exclusion keys stay
                        // stable. Virtual albums' records update too.
                        let title = title.trim().to_string();
                        if !title.is_empty() {
                            if let Some(a) = albums
                                .iter_mut()
                                .find(|a| a.dir.to_string_lossy() == dir)
                            {
                                a.title = title.clone();
                            }
                            let lib = &mut settings.library;
                            lib.album_titles.insert(dir.clone(), title.clone());
                            if let Some(ia) = lib
                                .virtual_albums
                                .iter_mut()
                                .find(|ia| ia.dir == dir)
                            {
                                ia.title = title.clone();
                            }
                            if carousel.album_key() == Some(dir.as_str()) {
                                carousel.set_album_title(&title);
                            }
                            settings::save(settings);
                        }
                    }
                    album_row::Action::Opened(i) => {
                        // Double-click loads the album's tracks into the
                        // cover view (Lacrima's LoadedAlbum -> CoverItems);
                        // playback only starts when a track item itself
                        // is double-clicked. The spectrum cover gradient
                        // follows the album's top cover.
                        if let Some(album) = albums.get(i) {
                            // The top cover feeds the spectrum gradient +
                            // shadertoy palette — decode it off the UI
                            // thread: a Web source is a network fetch
                            // (seconds), which used to freeze the whole
                            // view (and stall track-end advance).
                            if let Some(src) = album.covers.last().cloned() {
                                let dir =
                                    album.dir.to_string_lossy().into_owned();
                                let (tx, rx) = std::sync::mpsc::channel();
                                std::thread::Builder::new()
                                    .name("aes-album-cover".into())
                                    .spawn(move || {
                                        let _ = tx.send((
                                            dir,
                                            library::decode_cover(&src),
                                        ));
                                    })
                                    .ok();
                                *album_cover_rx = Some(rx);
                            } else {
                                *album_cover_rx = None;
                            }
                            // The album's persisted url items append
                            // after its file tracks (Lacrima's album
                            // Children contain both kinds).
                            let urls = settings
                                .library
                                .url_items
                                .get(&album.dir.to_string_lossy().into_owned())
                                .cloned()
                                .unwrap_or_default();
                            let (tracks, title) = carousel.open(album, &urls);
                            *url_info_rx =
                                Some(library::spawn_url_info(tracks, title));
                        }
                    }
                    album_row::Action::Selected(_) | album_row::Action::None => {}
                }
                // Stream end -> RepeatMode: One replays the track, All
                // auto-advances through the loaded album (wrapping), Off
                // advances without wrapping and stops at the last item.
                if play_path.is_none() {
                    if let (Some(p), Some(cur)) = (player.as_ref(), playing_track.as_ref()) {
                        let mut end_hit = p.state() == audio::PlayerState::Finished;
                        // Lacrima's OnTimeChangedForSilence: once the
                        // position enters the waveform-measured
                        // trailing-silence region, wait
                        // SilenceAdvanceDelayMs then end the track
                        // early. Repeat-One is exempt — its loop
                        // replays the tail instead of advancing.
                        if !end_hit
                            && *trailing_silence_s > 0.0
                            && transport.repeat != transport::RepeatMode::One
                            && p.state() == audio::PlayerState::Playing
                        {
                            let in_tail = p.position().as_secs_f64()
                                >= p.duration().as_secs_f64() - *trailing_silence_s;
                            if in_tail {
                                let t0 = silence_entered
                                    .get_or_insert_with(std::time::Instant::now);
                                if t0.elapsed().as_millis() as u64
                                    >= settings.audio.silence_advance_delay_ms as u64
                                {
                                    end_hit = true;
                                }
                            } else {
                                // Seeked back out before the delay
                                // elapsed — re-arm the trigger.
                                *silence_entered = None;
                            }
                        }
                        if end_hit {
                            match transport.repeat {
                                transport::RepeatMode::One => p.seek(0.0),
                                transport::RepeatMode::All => {
                                    play_path = carousel.step_path(cur, 1, true);
                                }
                                transport::RepeatMode::Off => {
                                    play_path = carousel.step_path(cur, 1, false);
                                }
                            }
                        }
                    }
                }
                if let Some(track) = play_path {
                    // The new playing item owns the selection and gets
                    // scrolled into view (transport steps land here too).
                    carousel.reveal(&track);
                    // Double-clicked a cover item — play that track
                    // (Lacrima's ItemDoubleClickedCommand ->
                    // OpenSelectedItem). The gradient follows the
                    // track's own art.
                    if library::is_url(&track) {
                        // Online item: resolve the direct stream url
                        // with yt-dlp on a worker — Lacrima's
                        // ResolveMediaSourceAsync (bestaudio first,
                        // muxed `best` fallback). Playback starts when
                        // resolve_rx lands.
                        let url = track.to_string_lossy().into_owned();
                        let item = track.clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::Builder::new()
                            .name("aes-stream-resolve".into())
                            .spawn(move || {
                                let one = |sel: &str| {
                                    ytdlp::stream_urls(
                                        &url,
                                        sel,
                                        ytdlp::ExtractorProfile::Default,
                                    )
                                    .ok()
                                    .and_then(|v| v.into_iter().next())
                                };
                                // Symphonia has no Opus decoder, so
                                // `bestaudio` (YT's webm/opus) is
                                // useless to us — prefer m4a/AAC, then
                                // any non-opus direct-progressive
                                // stream (protocol^=http also excludes
                                // HLS/DASH manifests symphonia can't
                                // parse). `best` muxed last resort.
                                let sel = "ba[ext=m4a][protocol^=http]/ba[acodec!=opus][protocol^=http]/b[protocol^=http]";
                                let _ = tx.send((
                                    item,
                                    one(sel)
                                        .or_else(|| one("b"))
                                        .ok_or_else(|| {
                                            "no playable stream".to_string()
                                        }),
                                ));
                            })
                            .ok();
                        *resolve_rx = Some(rx);
                        // Remote cover fetch+decode off the UI thread.
                        if let Some(src) = carousel.cover_for(&track) {
                            let (tx, rx) = std::sync::mpsc::channel();
                            let t = track.clone();
                            std::thread::Builder::new()
                                .name("aes-cover-fetch".into())
                                .spawn(move || {
                                    let _ =
                                        tx.send((t, library::decode_cover(&src)));
                                })
                                .ok();
                            *playing_cover_rx = Some(rx);
                        }
                        *playing_track = Some(track.clone());
                        if let Some(c) = waveform_cancel.take() {
                            c.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        waveform.clear();
                        waveform_shown.clear();
                        *trailing_silence_s = 0.0;
                        *silence_entered = None;
                        // Spinner until the stream url resolves and the
                        // decoder is actually running (the waveform row
                        // has no local file to generate from).
                        *waveform_loading = true;
                        *waveform_rx = None;
                    } else {
                        let img =
                            library::decode_cover(&library::CoverSrc::Tag(track.clone()));
                        *spectrum_colors =
                            img.as_ref().map(|i| spectrum_bar::dominant_colors(i, 3));
                        if let (Some(toy), Some(img)) = (shadertoy.as_mut(), img.as_ref()) {
                            toy.set_cover_image(img);
                            if let Some(c) = spectrum_colors.as_deref() {
                                toy.set_palette(c);
                            }
                        }
                        // The turntable's vinyl label
                        // (PlaybackCoordinator.ActiveCoverBitmap).
                        if let Some(img) = &img {
                            *playing_cover = Some(ui.ctx().load_texture(
                                "player-cover",
                                img.clone(),
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        if player.is_none() {
                            *player = audio::AudioPlayer::new().ok();
                            // A lazily created player starts at unity —
                            // apply the persisted EQ curve.
                            if let Some(p) = player {
                                for (i, &db) in
                                    settings.equalizer.bands.iter().enumerate()
                                {
                                    p.set_band_gain(i, db);
                                }
                                p.apply_audio(&settings.audio);
                            }
                        }
                        if let Some(p) = player {
                            let _ = p.open(&track);
                        }
                        *playing_track = Some(track.clone());
                        if let Some(c) = waveform_cancel.take() {
                            c.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        waveform.clear();
                        waveform_shown.clear();
                        *trailing_silence_s = 0.0;
                        *silence_entered = None;
                        *waveform_loading = true;
                        let cancel =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        *waveform_rx =
                            Some(audio::waveform::spawn(track, cancel.clone()));
                        *waveform_cancel = Some(cancel);
                    }
                }
                // The Add Url / Add Playlist prompts (Lacrima's
                // IsAddUrlPopupOpen overlays on the cover view).
                if let Some(kind) = prompt_kind {
                    url_prompt.open(kind, &ui.ctx().clone());
                }
                // "Delete" on a pointed item or the Delete key on the
                // card selection — Lacrima's DeletePointedItem:
                // remove from the view and stop playback when a
                // removed item was the current one. Unconfirmed
                // requests open the confirmation dialog first.
                let confirmed_items = if items_confirmed {
                    delete_path.take()
                } else {
                    if let Some(paths) = delete_path {
                        del_prompt.request(PendingDelete::Items(paths), ui.ctx());
                    }
                    None
                };
                if let Some(paths) = confirmed_items {
                    if let Some(cur) = playing_track.as_ref() {
                        if paths.contains(cur) {
                            if let Some(p) = player {
                                p.stop();
                            }
                            *playing_track = None;
                            *playing_cover = None;
                            *waveform_loading = false;
                        }
                    }
                    // A deleted stream item's caches go with it —
                    // stream ranges, resolved labels, thumbnails.
                    for path in &paths {
                        if library::is_url(path) {
                            evict_url_caches(&path.to_string_lossy());
                        }
                    }
                    carousel.remove_many(&paths);
                    // Persist file-item deletions: the scanner would
                    // otherwise re-add them on the next scan (Lacrima
                    // serializes the reduced Children list; we carry
                    // the removal in Settings.Library.ExcludedTracks).
                    if let Some(key) =
                        carousel.album_key().map(|k| k.to_string())
                    {
                        let removed: Vec<String> = paths
                            .iter()
                            .filter(|p| !library::is_url(p))
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect();
                        if !removed.is_empty() {
                            settings
                                .library
                                .excluded_tracks
                                .entry(key.clone())
                                .or_default()
                                .extend(removed);
                            settings::save(settings);
                        }
                        // The tile's count badge = tracks + url items —
                        // drop removed file items from `tracks` too (url
                        // items update via sync_url_covers in persist).
                        if let Some(a) = albums
                            .iter_mut()
                            .find(|a| a.dir.to_string_lossy() == key)
                        {
                            a.tracks.retain(|t| !paths.contains(t));
                        }
                    }
                    persist_url_items(carousel, albums, settings);
                    ui.ctx().request_repaint();
                }
                // "Clear Album" — Lacrima's ClearAlbumCommand: empty
                // the loaded album's children and stop playback when
                // the current item belonged to it. The album stays
                // loaded (Add Url still applies to it).
                if clear_album {
                    if let Some(cur) = playing_track.as_ref() {
                        if carousel.index_of(cur).is_some() {
                            if let Some(p) = player {
                                p.stop();
                            }
                            *playing_track = None;
                            *playing_cover = None;
                            *waveform_loading = false;
                        }
                    }
                    // Cleared url items' caches are orphaned the same
                    // way a single Delete's are — evict them all.
                    for u in carousel.url_items() {
                        evict_url_caches(&u);
                    }
                    // File items persist the same way a Delete does —
                    // excluded_tracks keeps them out of the next scan.
                    let file_paths = carousel.file_items();
                    carousel.clear_items();
                    if let Some(key) =
                        carousel.album_key().map(|k| k.to_string())
                    {
                        if !file_paths.is_empty() {
                            settings
                                .library
                                .excluded_tracks
                                .entry(key.clone())
                                .or_default()
                                .extend(
                                    file_paths
                                        .iter()
                                        .map(|p| {
                                            p.to_string_lossy().into_owned()
                                        })
                                        .collect::<Vec<_>>(),
                                );
                            settings::save(settings);
                        }
                        // Count badge: every child left the view.
                        if let Some(a) = albums
                            .iter_mut()
                            .find(|a| a.dir.to_string_lossy() == key)
                        {
                            a.tracks.clear();
                        }
                    }
                    persist_url_items(carousel, albums, settings);
                    ui.ctx().request_repaint();
                }
                // "Export Playlist" — the loaded album's URL items to
                // an .apl file (Lacrima's ExportPlaylistAsync, online
                // items only).
                if export_album {
                    let title = carousel.album_title();
                    let file_name = {
                        let n = apl::sanitize_file_name(&title);
                        if n.is_empty() {
                            "playlist".to_string()
                        } else {
                            n
                        }
                    };
                    if let Some(file) = rfd::FileDialog::new()
                        .set_title("Export Online Playlist")
                        .set_file_name(format!("{file_name}.apl"))
                        .add_filter("AES Online Playlist", &["apl"])
                        .save_file()
                    {
                        let items = carousel.export_items();
                        if let Err(e) = apl::write(&file, &title, items) {
                            eprintln!("apl export failed: {e}");
                        }
                    }
                }
                // "Edit Metadata" on a track item -> the tag editor
                // modal (Lacrima's OpenMetadataCommand).
                if let Some(path) = edit_path {
                    tag_editor.open(&ui.ctx().clone(), path);
                }
                // A finished save: refresh the item labels, drop the
                // cached cover so it re-decodes, and re-read the disc
                // art if it's the playing track. Stream items edit the
                // local meta cache — refresh goes through the url-info
                // path and the cover override file.
                if let Some(saved) = tag_editor.take_saved() {
                    if library::is_url(&saved) {
                        let url = saved.to_string_lossy().into_owned();
                        // Drop every texture keyed on cover sources this
                        // item may use: the override file, the guessed
                        // ladder, and the resolved thumb src.
                        album_row.invalidate_cover(&library::CoverSrc::File(
                            crate::meta_cache::cover_path(&url),
                        ));
                        if let Some(src) = library::CoverSrc::web_for(&url) {
                            album_row.invalidate_cover(&src);
                        }
                        if let Some(t) =
                            crate::meta_cache::load(&url).and_then(|m| m.thumb)
                        {
                            album_row
                                .invalidate_cover(&library::CoverSrc::Web(vec![t]));
                        }
                        if let Some((tracks, title)) = carousel.info_refresh_args()
                        {
                            *url_info_rx = Some(library::spawn_url_info(tracks, title));
                        }
                        // Re-read the now-playing art off the UI thread —
                        // without an override this is a remote fetch.
                        if playing_track.as_deref() == Some(saved.as_path()) {
                            let src = library::CoverSrc::File(
                                crate::meta_cache::cover_path(&url),
                            );
                            let src = if crate::meta_cache::cover_path(&url).exists()
                            {
                                Some(src)
                            } else {
                                library::CoverSrc::web_for(&url)
                            };
                            if let Some(src) = src {
                                let (tx, rx) = std::sync::mpsc::channel();
                                let t = saved.clone();
                                std::thread::Builder::new()
                                    .name("aes-cover-fetch".into())
                                    .spawn(move || {
                                        let _ =
                                            tx.send((t, library::decode_cover(&src)));
                                    })
                                    .ok();
                                *playing_cover_rx = Some(rx);
                            }
                        }
                    } else {
                        album_row.invalidate_cover(&library::CoverSrc::Tag(saved.clone()));
                        if let Some((tracks, title)) = carousel.info_refresh_args() {
                            *track_info_rx =
                                Some(library::spawn_track_info(tracks, title));
                        }
                        if playing_track.as_deref() == Some(saved.as_path()) {
                            if let Some(img) = library::decode_cover(
                                &library::CoverSrc::Tag(saved.clone()),
                            ) {
                                *playing_cover = Some(ui.ctx().load_texture(
                                    "playing-cover",
                                    img,
                                    egui::TextureOptions::LINEAR,
                                ));
                            }
                        }
                    }
                }
                // The transport's cover-layout toggle persists
                // CoverLayoutMode.
                if settings.carousel.layout_mode != transport.cover_layout {
                    settings.carousel.layout_mode = transport.cover_layout;
                }
                if let Some(t) = seek_secs {
                    if let Some(p) = player {
                        p.seek(t);
                    }
                }

                // Settings overlay slides in from the right over whatever
                // view is showing (Lacrima's SettingsOverlay, not a view).
                let fx_before = (
                    settings.effects.show_lacrima,
                    settings.effects.lacrima_count,
                    settings.effects.show_edge_border,
                    settings.effects.animate_title,
                    settings.effects.show_shadertoy,
                    settings.effects.shadertoy.clone(),
                    settings.ui.scale_factor,
                    settings.spectrum.clone(),
                    settings.carousel.clone(),
                    settings.audio.clone(),
                    settings.emulation.clone(),
                    settings.effects.lacrima_cover_color,
                );
                let slide = overlay_slide;
                if slide > 0.001 {
                    settings_overlay(
                        ui,
                        content,
                        ease_out_cubic(slide),
                        radius,
                        settings_tab,
                        settings_open,
                        settings,
                        transport,
                        ytdlp,
                        emu,
                        shadertoy.as_ref().map(|t| t.names()).unwrap_or(&[]),
                        s,
                    );
                }
                if *settings_open && ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
                    *settings_open = false;
                }

                // Tag editor modal — topmost content layer (below the
                // rain/border window effects).
                if tag_editor.is_open() {
                    tag_editor.ui(ui, content, s);
                }
                // Add Url / Add Playlist prompts — same overlay spot
                // (Lacrima's popup layer above the content scrim).
                if url_prompt.is_open() {
                    if let Some((kind, url)) = url_prompt.ui(ui, content, s) {
                        match kind {
                            url_prompt::Kind::Url => {
                                if carousel.add_url(&url) {
                                    persist_url_items(carousel, albums, settings);
                                    let title = carousel
                                        .display_for(std::path::Path::new(&url))
                                        .2;
                                    *url_info_rx = Some(library::spawn_url_info(
                                        vec![std::path::PathBuf::from(url)],
                                        title,
                                    ));
                                    ui.ctx().request_repaint();
                                }
                            }
                            url_prompt::Kind::Playlist => {
                                // Expand the playlist on a worker —
                                // Lacrima's GetPlaylistVideoUrls page
                                // scrape; members land on playlist_rx.
                                url_prompt.playlist_busy = true;
                                let (tx, rx) = std::sync::mpsc::channel();
                                std::thread::Builder::new()
                                    .name("aes-playlist-expand".into())
                                    .spawn(move || {
                                        let _ =
                                            tx.send(ytdlp::playlist_video_urls(&url));
                                    })
                                    .ok();
                                *playlist_rx = Some(rx);
                            }
                        }
                    }
                }

                // The Lacrima rain layer sits above everything inside the
                // window border (content, title bar, settings overlay),
                // like Lacrima's ParticleLayer.
                if settings.effects.show_lacrima {
                    lacrima.set_count(settings.effects.lacrima_count);
                    lacrima.update(dt);
                    let clip = frame_rect.shrink(1.0);
                    let painter = ui.painter().with_clip_rect(clip);
                    let tint = if settings.effects.lacrima_cover_color {
                        spectrum_colors.as_deref()
                    } else {
                        None
                    };
                    lacrima.paint(&painter, clip, tint);
                    ui.ctx().request_repaint();
                }
                // Outermost layer: the glowing edge border around the
                // window, like Lacrima's EdgeBorderLayer.
                if settings.effects.show_edge_border {
                    edge_border::paint(
                        ui.painter(),
                        frame_rect.shrink(0.5),
                        (f32::from(radius) - 0.5).max(0.0),
                        elapsed,
                    );
                    ui.ctx().request_repaint();
                }
                // Startup fade-in veil, above everything.
                if boot_t < 1.0 {
                    ui.painter().rect_filled(
                        frame_rect,
                        CornerRadius::same(radius),
                        Color32::BLACK.gamma_multiply(1.0 - boot_t),
                    );
                    ui.ctx().request_repaint();
                }
                let fx_changed = fx_before
                    != (
                        settings.effects.show_lacrima,
                        settings.effects.lacrima_count,
                        settings.effects.show_edge_border,
                        settings.effects.animate_title,
                        settings.effects.show_shadertoy,
                        settings.effects.shadertoy.clone(),
                        settings.ui.scale_factor,
                        settings.spectrum.clone(),
                        settings.carousel.clone(),
                        settings.audio.clone(),
                        settings.emulation.clone(),
                        settings.effects.lacrima_cover_color,
                    )
                    || settings.equalizer.bands != eq_before;
                // Audio-tab changes apply live to the player — Lacrima
                // mirrors the same set onto AudioPlayer and recomputes
                // the current track's ReplayGain on every change.
                if settings.audio != fx_before.9 {
                    if let Some(p) = player {
                        p.apply_audio(&settings.audio);
                    }
                }

                // Sync the settings mirror; save when a drag finishes or the
                // layout changed while not dragging (menu actions, reset).
                // Offset persists as the widget CENTER's fraction of
                // the whole WINDOW so a widget keeps its screen spot
                // through fullscreen transitions (the title bar hides
                // there); size persists in absolute virtual units so
                // fullscreen never inflates widgets.
                // anchor_offset evaluates the canonical intent — a
                // clamped/transient display rect can't leak into the
                // save (the source of the random shift on launches).
                let off = clock.anchor_offset(content, frame_rect);
                let wsize = clock.rect.size();
                let differs = settings.clock.left != off.x
                    || settings.clock.top != off.y
                    || settings.clock.width != wsize.x
                    || settings.clock.height != wsize.y
                    || settings.clock.visible != clock.visible;
                settings.clock.left = off.x;
                settings.clock.top = off.y;
                settings.clock.width = wsize.x;
                settings.clock.height = wsize.y;
                settings.clock.visible = clock.visible;
                // The turntable widget persists the same way.
                let poff = player_widget.anchor_offset(content, frame_rect);
                let psize = player_widget.rect.size();
                let pdiffers = settings.player.left != poff.x
                    || settings.player.top != poff.y
                    || settings.player.width != psize.x
                    || settings.player.height != psize.y
                    || settings.player.visible != player_widget.visible;
                settings.player.left = poff.x;
                settings.player.top = poff.y;
                settings.player.width = psize.x;
                settings.player.height = psize.y;
                settings.player.visible = player_widget.visible;
                // Resizing the window re-anchors the widget every frame, so
                // rate-limit writes; drag ends and menu changes save at
                // once. Defer while any pointer press is held — a
                // synchronous file write mid slider/EQ drag stalls a frame.
                let save_due = last_save.elapsed().as_millis() > 500;
                let held = ui.ctx().input(|i| i.pointer.primary_down());
                if ended
                    || (!held
                        && !clock.dragging
                        && !player_widget.dragging
                        && (differs || pdiffers || fx_changed)
                        && save_due)
                {
                    settings::save(settings);
                    *last_save = std::time::Instant::now();
                }

                // Switch-style exit-fullscreen confirmation — the
                // last thing drawn so it covers all content. `open`
                // flips off on Cancel/Exit; `vis` trails it to 0 so
                // the overlay fades out, and the actual fullscreen
                // leave waits for the fade to finish.
                if fs_exit.open || fs_exit.vis > 0.001 {
                    let dt = ui.ctx().input(|i| i.stable_dt);
                    fs_exit.vis = (fs_exit.vis
                        + dt / if fs_exit.open { 0.18 } else { -0.14 })
                    .clamp(0.0, 1.0);
                    match confirm_overlay(
                        ui,
                        content,
                        "Exit fullscreen?",
                        "Are you sure you want to leave fullscreen mode?",
                        "Exit",
                        "Cancel",
                        &mut fs_exit.sel,
                        s,
                        fs_exit.vis,
                        fs_exit.open,
                        "fs-exit",
                    ) {
                        DialogAction::Confirm => {
                            fs_exit.open = false;
                            fs_exit.pending_exit = true;
                        }
                        DialogAction::Cancel => fs_exit.open = false,
                        DialogAction::None => {}
                    }
                    ui.ctx().request_repaint();
                    if fs_exit.pending_exit && fs_exit.vis <= 0.001 {
                        fs_exit.pending_exit = false;
                        ui.ctx()
                            .send_viewport_cmd(ViewportCommand::Fullscreen(false));
                    }
                }
                // Delete confirmation — same dialog, payload parked in
                // `pending` and re-injected next frame on confirm.
                if del_prompt.open || del_prompt.vis > 0.001 {
                    let dtp = ui.ctx().input(|i| i.stable_dt);
                    del_prompt.vis = (del_prompt.vis
                        + dtp / if del_prompt.open { 0.18 } else { -0.14 })
                    .clamp(0.0, 1.0);
                    let (title, subtitle) = match &del_prompt.pending {
                        Some(PendingDelete::Items(p)) => (
                            if p.len() == 1 {
                                "Delete item?".to_string()
                            } else {
                                format!("Delete {} items?", p.len())
                            },
                            "Removed from the album — the files stay on disk."
                                .to_string(),
                        ),
                        Some(PendingDelete::Albums(d)) => (
                            if d.len() == 1 {
                                "Delete album?".to_string()
                            } else {
                                format!("Delete {} albums?", d.len())
                            },
                            "Removed from the library — the files stay on disk."
                                .to_string(),
                        ),
                        Some(PendingDelete::Roms(p)) => (
                            if p.len() == 1 {
                                "Remove item?".to_string()
                            } else {
                                format!("Remove {} items?", p.len())
                            },
                            "Removed from the section — the files stay on disk."
                                .to_string(),
                        ),
                        None => ("Delete?".to_string(), String::new()),
                    };
                    match confirm_overlay(
                        ui,
                        content,
                        &title,
                        &subtitle,
                        "Delete",
                        "Cancel",
                        &mut del_prompt.sel,
                        s,
                        del_prompt.vis,
                        del_prompt.open,
                        "del-confirm",
                    ) {
                        DialogAction::Confirm => {
                            del_prompt.open = false;
                            del_prompt.confirmed = del_prompt.pending.take();
                        }
                        DialogAction::Cancel => del_prompt.dismiss(),
                        DialogAction::None => {}
                    }
                    ui.ctx().request_repaint();
                }
            },
        );
        if fs_exit_req {
            fs_exit.begin(ui.ctx());
        }
        if back_clicked || esc_back {
            if self.url_prompt.is_open() {
                self.url_prompt.dismiss();
            } else if self.tag_editor.is_open() {
                self.tag_editor.close();
            } else {
                self.nav.request(View::Main);
            }
        }
        if gear_clicked {
            self.settings_open = !self.settings_open;
        }
        {
            static FRAME2: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            let n = FRAME2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 3 {
                boot_mark(&format!("frame {n} ui() done"));
            }
        }
    }

    fn on_exit(&mut self) {
        settings::save(&self.settings);
    }
}

/// Right-click context menu for the content area and widgets, mirroring
/// Lacrima's view menu.
fn widget_menu(
    ui: &mut egui::Ui,
    clock: &mut widgets::WidgetFrame,
    player: &mut widgets::WidgetFrame,
    widget_edit: &mut bool,
    content: Rect,
    frame: Rect,
    s: f32,
) {
    if ui
        .button(if clock.visible { "Hide Clock" } else { "Show Clock" })
        .clicked()
    {
        clock.visible = !clock.visible;
        ui.close();
    }
    if ui
        .button(if player.visible { "Hide Player" } else { "Show Player" })
        .clicked()
    {
        player.visible = !player.visible;
        ui.close();
    }
    ui.checkbox(widget_edit, "Widget Edit Mode");
    if ui.button("Reset Widget Layout").clicked() {
        clock.reset(content, frame, s);
        player.reset(content, frame, s);
        ui.close();
    }
}

/// Frameless transparent window whose content is painted inside a rounded
/// border, matching the AES_Lacrima window chrome:
/// `CornerRadius=10`, `BorderBrush=DimGray`, `BorderThickness=0.5`, black fill.
fn lacrima_window_frame(
    ui: &mut egui::Ui,
    title: &str,
    logo: &logo::TitleLogo,
    elapsed: f32,
    back_icon: egui::TextureId,
    back_alpha: f32,
    back_clicked: &mut bool,
    settings_icon: egui::TextureId,
    settings_active: bool,
    gear_clicked: &mut bool,
    animate_title: bool,
    scale: f32,
    resize_exclude: Option<Rect>,
    fs_exit_req: &mut bool,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let ctx = ui.ctx().clone();
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    let radius = if maximized || is_fullscreen {
        0
    } else {
        CORNER_RADIUS
    };
    let corner_radius = CornerRadius::same(radius);

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(Color32::TRANSPARENT)
                .corner_radius(corner_radius)
                .stroke(if maximized || is_fullscreen {
                    Stroke::NONE
                } else {
                    Stroke::new(1.0, Color32::from_rgb(105, 105, 105))
                }),
        )
        .show(ui, |ui| {
            let app_rect = ui.max_rect();

            let resize_dir = resize_direction(
                &ctx,
                app_rect,
                maximized || is_fullscreen,
                resize_exclude,
            );
            if let Some(dir) = resize_dir {
                ctx.set_cursor_icon(cursor_icon_for(dir));
                if ctx.input(|i| i.pointer.button_pressed(PointerButton::Primary)) {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
                }
            }

            let title_bar_rect = Rect::from_min_max(
                app_rect.min,
                Pos2::new(
                    app_rect.max.x,
                    app_rect.min.y
                        + if is_fullscreen {
                            0.0
                        } else {
                            TITLE_BAR_HEIGHT * scale
                        },
                ),
            );
            if !is_fullscreen {
                let title_bar_response = ui.interact(
                    title_bar_rect,
                    ui.id().with("title_bar"),
                    Sense::click_and_drag(),
                );
                if resize_dir.is_none()
                    && title_bar_response.drag_started_by(PointerButton::Primary)
                {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                if title_bar_response.double_clicked_by(PointerButton::Primary) {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                }
            }

            add_contents(ui);

            // Fullscreen renders content only — no bar, title, or
            // window buttons (Esc exits fullscreen; Esc/Backspace
            // still navigate back).
            if is_fullscreen {
                return;
            }

            // The bar is an overlay IN FRONT of the content, like
            // Lacrima's MainTopBar sharing the root grid cell with a
            // higher ZIndex — painting it AFTER the content (same layer)
            // makes anything that slides up (EQ drawer lifting the cover
            // view, scrolled cards) pass behind its mostly-opaque fill
            // instead of drawing over it. A foreground LAYER would do
            // the same visually but blocks hit-testing for every widget
            // underneath it, so paint order is used instead.
            let painter = ui.painter().clone().with_clip_rect(app_rect);
            // Dark, mostly-opaque bar like Lacrima's MainTopBar
            // (Background="Black" Opacity="0.8"), top corners rounded.
            painter.rect_filled(
                title_bar_rect,
                CornerRadius {
                    nw: radius,
                    ne: radius,
                    se: 0,
                    sw: 0,
                },
                Color32::from_black_alpha(204),
            );

            logo.paint(&painter, title_bar_rect, elapsed, title, animate_title);

            // Back button at the top-left of the bar, like Lacrima's
            // arrow button bound to NavigationService.NavigateBackCommand.
            if back_alpha > 0.0 {
                // 35x30 button, 15px left margin, 24px icon (MainTopBar).
                let btn = Rect::from_min_size(
                    Pos2::new(
                        title_bar_rect.min.x + 15.0 * scale,
                        title_bar_rect.min.y + (title_bar_rect.height() - 30.0 * scale) / 2.0,
                    ),
                    Vec2::new(35.0 * scale, 30.0 * scale),
                );
                let back_resp =
                    ui.interact(btn, ui.id().with("back"), Sense::click());
                if back_resp.hovered() {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                    painter.rect_filled(
                        btn,
                        CornerRadius::same((6.0 * scale).round() as u8),
                        Color32::from_white_alpha((20.0 * back_alpha) as u8),
                    );
                }
                painter.image(
                    back_icon,
                    Rect::from_center_size(btn.center(), Vec2::splat(24.0 * scale)),
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE.gamma_multiply(back_alpha),
                );
                if back_resp.clicked() {
                    *back_clicked = true;
                }
            }

            painter.line_segment(
                [
                    Pos2::new(app_rect.min.x, title_bar_rect.max.y),
                    title_bar_rect.right_bottom(),
                ],
                Stroke::new((1.5 * scale).max(1.0), Color32::from_gray(85)),
            );

            ui.scope_builder(UiBuilder::new().max_rect(title_bar_rect), |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Buttons carry Margin=10 in a Spacing=5 StackPanel:
                    // 10px right edge, 25px between buttons.
                    ui.spacing_mut().item_spacing.x = 25.0 * scale;
                    ui.add_space(10.0 * scale);
                    if title_button(ui, TitleButton::Close, scale).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    if title_button(ui, TitleButton::Minimize, scale).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                    let kind = if is_fullscreen {
                        TitleButton::FullscreenExit
                    } else {
                        TitleButton::Fullscreen
                    };
                    if title_button(ui, kind, scale).clicked() {
                        // Exiting fullscreen goes through the
                        // confirmation prompt like Esc does.
                        if is_fullscreen {
                            *fs_exit_req = true;
                        } else {
                            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
                        }
                    }
                    // Settings gear, left of the window buttons like
                    // Lacrima's MainTopBar: dimmed at 0.5 opacity normally,
                    // full while the overlay is open. Toggles the same
                    // overlay as the menu gear, from any view.
                    let (rect, gear) = ui
                        .allocate_exact_size(Vec2::new(35.0 * scale, 30.0 * scale), Sense::click());
                    let lit = settings_active || gear.hovered();
                    if gear.hovered() {
                        ctx.set_cursor_icon(CursorIcon::PointingHand);
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same((6.0 * scale).round() as u8),
                            Color32::from_white_alpha(20),
                        );
                    }
                    // Rotate the gear 90° while the overlay slides in (same
                    // 0.3s easeOutCubic), back to 0° as it closes.
                    let rot = ctx.animate_bool_with_time(
                        ui.id().with("gear-rot"),
                        settings_active,
                        0.3,
                    );
                    let angle = ease_out_cubic(rot) * std::f32::consts::FRAC_PI_2;
                    egui::Image::new(egui::load::SizedTexture::new(
                        settings_icon,
                        Vec2::splat(28.0 * scale),
                    ))
                    .rotate(angle, Vec2::splat(0.5))
                    .tint(Color32::WHITE.gamma_multiply(if lit { 1.0 } else { 0.5 }))
                    .paint_at(
                        ui,
                        Rect::from_center_size(rect.center(), Vec2::splat(28.0 * scale)),
                    );
                    if gear.clicked() {
                        *gear_clicked = true;
                    }
                    gear.on_hover_text("Settings");
                });
            });
        });
}

fn resize_direction(
    ctx: &egui::Context,
    rect: Rect,
    maximized: bool,
    exclude: Option<Rect>,
) -> Option<ResizeDirection> {
    if maximized {
        return None;
    }
    let pos = ctx.input(|i| i.pointer.hover_pos())?;
    if !rect.contains(pos) {
        return None;
    }
    // Interactive bottom-edge content (the album strip + its scrollbar)
    // wins over the resize grip — otherwise BeginResize steals the drag.
    if exclude.map(|r| r.contains(pos)).unwrap_or(false) {
        return None;
    }
    let left = pos.x < rect.min.x + RESIZE_BORDER;
    let right = pos.x > rect.max.x - RESIZE_BORDER;
    let top = pos.y < rect.min.y + RESIZE_BORDER;
    let bottom = pos.y > rect.max.y - RESIZE_BORDER;
    match (top, bottom, left, right) {
        (true, false, true, false) => Some(ResizeDirection::NorthWest),
        (true, false, false, true) => Some(ResizeDirection::NorthEast),
        (false, true, true, false) => Some(ResizeDirection::SouthWest),
        (false, true, false, true) => Some(ResizeDirection::SouthEast),
        (true, false, false, false) => Some(ResizeDirection::North),
        (false, true, false, false) => Some(ResizeDirection::South),
        (false, false, true, false) => Some(ResizeDirection::West),
        (false, false, false, true) => Some(ResizeDirection::East),
        _ => None,
    }
}

fn cursor_icon_for(dir: ResizeDirection) -> CursorIcon {
    match dir {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
    }
}

// Main menu, `MainMenuView.axaml` virtual units: label (FontSize 24,
// margin-bottom -15) stacked over a 120x120 icon button (70px icon), the
// row overlapping the 700x40 dock bar by 10 (StackPanel spacing -10).
const MENU_DOCK_HEIGHT: f32 = 40.0;
const MENU_DOCK_WIDTH: f32 = 700.0;
const MENU_ITEM_WIDTH: f32 = 120.0;
const MENU_ITEM_SPACING: f32 = 12.0;
// Lacrima's label block: FontSize 24 + Label padding ≈ 38px tall, then
// the button overlaps it by 15 (its margin-bottom is -15).
const MENU_LABEL_HEIGHT: f32 = 38.0;
const MENU_LABEL_OVERLAP: f32 = 15.0;
const MENU_LABEL_FONT: f32 = 24.0;
const MENU_BUTTON_HEIGHT: f32 = 120.0;
const MENU_ICON_SIZE: f32 = 70.0;
const MENU_ITEM_HEIGHT: f32 =
    MENU_LABEL_HEIGHT - MENU_LABEL_OVERLAP + MENU_BUTTON_HEIGHT;
const MENU_DOCK_OVERLAP: f32 = 10.0;

// Dock magnification: hovered icon grows toward this scale, influence on
// neighbors falls off as a gaussian over horizontal distance.
const MENU_MAX_SCALE: f32 = 1.3;
const MENU_SIGMA: f32 = MENU_ITEM_WIDTH * 0.9;
const MENU_SMOOTHING: f32 = 16.0;
const MENU_PRESS_SHRINK: f32 = 0.15;
// Fixed gap between the label's baseline slot and the icon's top edge —
// kept constant while the icon magnifies (the label rides up with it).
const MENU_LABEL_GAP: f32 = 17.0;

/// The main-menu entries in display order. The number keys bind
/// 1..=N in this order — a new entry here automatically picks up
/// the next number. The last entry is always Settings (toggles
/// the overlay instead of navigating).
fn menu_entries(
    icons: &icons::MenuIcons,
) -> [(&'static str, &egui::TextureHandle); 4] {
    [
        ("Emulation", &icons.emulation),
        ("Re/Decompilation", &icons.recomp),
        ("Music", &icons.music),
        ("Settings", &icons.settings),
    ]
}

/// One menu activation — click or number key alike.
fn menu_activate(i: usize, menu_len: usize, settings_open: &mut bool, nav: &mut Nav) {
    if i == menu_len - 1 {
        *settings_open = !*settings_open;
    } else if i < View::SUB_VIEWS.len() {
        nav.request(View::SUB_VIEWS[i]);
    }
}

fn main_menu(
    ui: &mut egui::Ui,
    icons: &icons::MenuIcons,
    scales: &mut [f32; 4],
    interactive: bool,
    settings_active: bool,
    s: f32,
) -> Option<usize> {
    let mut clicked = None;
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    let painter = ui.painter();

    // All dock metrics live inside the ScalableDecorator — virtual units
    // scaled by `s` (1/ScaleFactor).
    let dock_h = MENU_DOCK_HEIGHT * s;
    let item_w = MENU_ITEM_WIDTH * s;
    let item_h = MENU_ITEM_HEIGHT * s;
    let spacing = MENU_ITEM_SPACING * s;
    let icon_size = MENU_ICON_SIZE * s;
    let sigma = MENU_SIGMA * s;

    // Dock bar: white, top corners rounded, flush with the window bottom.
    // Keep it inside the window's rounded corners.
    let dock_w = (MENU_DOCK_WIDTH * s).min(rect.width() - 2.0 * f32::from(CORNER_RADIUS));
    let dock = Rect::from_min_size(
        Pos2::new(rect.center().x - dock_w / 2.0, rect.max.y - dock_h),
        Vec2::new(dock_w, dock_h),
    );
    painter.rect_filled(
        dock,
        CornerRadius {
            nw: (8.0 * s).round() as u8,
            ne: (8.0 * s).round() as u8,
            se: 0,
            sw: 0,
        },
        Color32::from_white_alpha(153),
    );

    let entries = menu_entries(icons);
    let row_w = item_w * entries.len() as f32 + spacing * (entries.len() - 1) as f32;
    let left = rect.center().x - row_w / 2.0;
    // Items overlap the dock by 10 (Lacrima's StackPanel Spacing="-10").
    let top = dock.min.y + MENU_DOCK_OVERLAP * s - item_h;

    // Gear spin, same 0.3s easeOutCubic as the top-bar gear and the panel.
    let gear_angle = ease_out_cubic(ctx.animate_bool_with_time(
        egui::Id::new("menu-gear-rot"),
        settings_active,
        0.3,
    )) * std::f32::consts::FRAC_PI_2;

    let pointer = ctx.input(|i| i.pointer.hover_pos());
    let dt = ctx.input(|i| i.stable_dt).min(0.1);
    let blend = 1.0 - (-dt * MENU_SMOOTHING).exp();
    let mut animating = false;

    for (i, (label, icon)) in entries.iter().enumerate() {
        let item_rect = Rect::from_min_size(
            Pos2::new(left + i as f32 * (item_w + spacing), top),
            Vec2::new(item_w, item_h),
        );
        let center_x = item_rect.center().x;

        let response = ui.interact(item_rect, ui.id().with(("main_menu", i)), Sense::click());
        if response.hovered() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        if interactive && response.clicked() {
            clicked = Some(i);
        }

        // Gaussian influence of the pointer over each item (1.0 = hovered).
        let influence = match pointer {
            Some(p) if p.y > item_rect.min.y - 20.0 * s && p.y < dock.max.y => {
                let dx = p.x - center_x;
                (-dx * dx / (2.0 * sigma * sigma)).exp()
            }
            _ => 0.0,
        };
        let target = 1.0 + (MENU_MAX_SCALE - 1.0) * influence;
        let scale = &mut scales[i];
        *scale += (target - *scale) * blend;
        animating |= influence > 0.0 || (target - *scale).abs() > 0.001;

        // While pressed it shrinks slightly for click feedback.
        let press = ctx.animate_bool_with_time(
            response.id.with("press"),
            response.is_pointer_button_down_on(),
            0.09,
        );
        // The icon is centered in the 120px button so magnification
        // grows symmetrically toward both edges.
        let icon_center_y = item_rect.max.y - MENU_BUTTON_HEIGHT / 2.0 * s;
        let size = icon_size * *scale * (1.0 - MENU_PRESS_SHRINK * press);
        let icon_rect = Rect::from_center_size(
            Pos2::new(center_x, icon_center_y),
            Vec2::splat(size),
        );
        let tint = Color32::WHITE;
        if i == 3 {
            // The dock gear spins 90° while the overlay opens, like the
            // top-bar gear; pivot is the texture center.
            egui::Image::new(egui::load::SizedTexture::new(
                icon.id(),
                icon_rect.size(),
            ))
            .rotate(gear_angle, Vec2::splat(0.5))
            .tint(tint)
            .paint_at(ui, icon_rect);
        } else {
            painter.image(
                icon.id(),
                icon_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                tint,
            );
        }

        // Labels fade in/out on item hover (0.18s DoubleTransition).
        // Anchored to the icon's top edge minus a fixed gap, so the
        // label rides up as the icon magnifies — the margin never changes.
        let opacity =
            ctx.animate_bool_with_time(response.id.with("label"), response.hovered(), 0.18);
        if opacity > 0.0 {
            painter.text(
                Pos2::new(center_x, icon_rect.min.y - MENU_LABEL_GAP * s),
                Align2::CENTER_BOTTOM,
                *label,
                FontId::proportional(MENU_LABEL_FONT * s),
                Color32::WHITE.gamma_multiply(opacity),
            );
        }
    }

    if animating {
        ctx.request_repaint();
    }
    clicked
}

/// MusicView.axaml chrome heights (scaled by `s`): the media-buttons
/// grid is Height=140 Background=#E6000000, the waveform row is the
/// 60px bar plus its 5/10 margins, and the album menu band
/// (#FF000000) wraps the 266px strip. Each boundary gets a 1px
/// Separator (Fluent's ~20% white).
const TRANSPORT_BAND_H: f32 = 140.0;
const WAVEFORM_BAR_H: f32 = 60.0;
const WAVEFORM_ROW_H: f32 = 75.0; // 60 + 5 top + 10 bottom margins
const BAND_BG: Color32 = Color32::from_black_alpha(247);
const GRID_BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);

/// The Music view: player chrome fills the transport band later; the
/// waveform seek bar sits directly above the albums strip
/// (Lacrima's MusicListView) like MusicView's grid rows 1/2/3.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn music_view(
    ui: &mut egui::Ui,
    content: Rect,
    alpha: f32,
    interactive: bool,
    albums: &mut Vec<library::Album>,
    row: &mut album_row::AlbumRow,
    carousel: &mut carousel::Carousel,
    player: Option<&audio::AudioPlayer>,
    playing_track: Option<&std::path::Path>,
    playing_loading: bool,
    waveform: &[f32],
    waveform_loading: bool,
    bar: &mut waveform_bar::WaveformBar,
    transport: &mut transport::Transport,
    spectrum: &spectrum_bar::SpectrumBar,
    spec_cfg: &settings::Spectrum,
    car_cfg: &settings::Carousel,
    eq: &mut settings::Equalizer,
    cover_colors: Option<&[Color32]>,
    accent: Color32,
    search: &mut search_pill::SearchPill,
    album_search: &mut search_pill::SearchPill,
    radius: u8,
    s: f32,
    scanning: bool,
    row_reorder_armed: bool,
    card_reorder_armed: bool,
) -> (
    album_row::Action,
    Option<f64>,
    Option<std::path::PathBuf>,
    Option<std::path::PathBuf>,
    Option<url_prompt::Kind>,
    Option<Vec<std::path::PathBuf>>,
    bool,
    bool,
) {
    let row_rect = album_row_rect(content, s);
    // Bottom-up stack: album menu band, waveform row, transport band.
    // The toggle button collapses the menu band — Lacrima's
    // MaxHeightToggleBehavior animates it over 0.55s CubicEaseInOut.
    let list_t = ease_in_out_cubic(ui.ctx().animate_bool_with_time(
        ui.id().with("albumlist-open"),
        transport.album_list_open,
        0.55,
    ));
    let menu_top = content.max.y - (content.max.y - row_rect.min.y) * list_t;
    let wave_top = (menu_top - WAVEFORM_ROW_H * s).max(content.min.y);
    // The EQ drawer opens between the transport band and the waveform
    // (AES-mini's MaxHeightToggleBehavior row): the band lifts on the
    // same cubic ease, uncovering the strip.
    // Exponential-damped reveal: max velocity at the start, smooth
    // asymptotic settle — symmetric on open/close and continuous when
    // toggled mid-flight (a reversed ease curve would pop).
    let eq_t = {
        let id = ui.id().with("eq-drawer");
        let mut t = ui.ctx().data_mut(|d| d.get_temp::<f32>(id)).unwrap_or(0.0);
        let target = if transport.equalizer_open { 1.0 } else { 0.0 };
        let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
        t += (target - t) * (1.0 - (-dt * 8.0).exp());
        if (target - t).abs() < 0.0015 {
            t = target;
        }
        ui.ctx().data_mut(|d| d.insert_temp(id, t));
        if t != target {
            ui.ctx().request_repaint();
        }
        t
    };
    let eq_top = wave_top - equalizer::PANEL_H * s * eq_t;
    let band_top = (eq_top - TRANSPORT_BAND_H * s).max(content.min.y);
    let band = Rect::from_min_max(
        Pos2::new(content.min.x, band_top),
        Pos2::new(content.max.x, eq_top),
    );
    let eq_rect = Rect::from_min_max(
        Pos2::new(content.min.x, eq_top),
        Pos2::new(content.max.x, wave_top),
    );
    let wave_row = Rect::from_min_max(
        Pos2::new(content.min.x, wave_top),
        Pos2::new(content.max.x, menu_top),
    );
    let menu_band = Rect::from_min_max(Pos2::new(content.min.x, menu_top), content.max);

    // The loaded album's track items fill the content row behind the
    // spectrum (MusicView's CompositionCoverControl, Grid.Row=0,
    // ClipToBounds=False — the chrome bands painted next cover any
    // overflow). Its interact widget registers before the pill/buttons
    // so they always win their own hit areas. The top-right search
    // pill's text filters the items (AlbumSearch -> CoverItems).
    let upper = Rect::from_min_max(content.min, Pos2::new(content.max.x, band.min.y));
    // MusicView's row-spanning Border paints the content row with
    // CardGridBackgroundColor in every mode, behind the cover view.
    ui.painter().rect_filled(
        upper,
        0,
        Color32::from_rgb(car_cfg.grid_bg[0], car_cfg.grid_bg[1], car_cfg.grid_bg[2])
            .gamma_multiply(alpha),
    );
    let cover_mode = match transport.cover_layout {
        1 => carousel::Mode::VerticalGrid,
        2 => carousel::Mode::HorizontalGrid,
        _ => carousel::Mode::Carousel,
    };
    // Grid modes lay out against the pre-lift area: when the EQ drawer
    // raises the transport band, the grid keeps its rows and the band
    // simply covers whatever slides under it (painted earlier).
    let grid_area = Rect::from_min_max(
        upper.min,
        Pos2::new(upper.max.x, upper.max.y + equalizer::PANEL_H * s * eq_t),
    );
    let carousel_action = carousel.ui(
        ui,
        upper,
        grid_area,
        row,
        playing_track,
        playing_loading,
        &search.text,
        car_cfg,
        cover_mode,
        alpha,
        interactive,
        card_reorder_armed,
        s,
    );

    let painter = ui.painter().clone();
    painter.rect_filled(band, 0.0, BAND_BG.gamma_multiply(alpha));
    if eq_t > 0.001 {
        // Same fill as the transport band so the drawer reads as one
        // lifted unit; the separators mark both its edges.
        painter.rect_filled(eq_rect, 0.0, BAND_BG.gamma_multiply(alpha));
    }
    painter.rect_filled(wave_row, 0.0, GRID_BG.gamma_multiply(alpha));
    if list_t > 0.001 {
        painter.rect_filled(
            menu_band,
            CornerRadius {
                nw: 0,
                ne: 0,
                sw: radius,
                se: radius,
            },
            Color32::BLACK.gamma_multiply(alpha),
        );
    }
    let line = Stroke::new(
        s.max(1.0),
        Color32::from_white_alpha((51.0 * alpha) as u8),
    );
    for y in [band.min.y, wave_row.min.y, menu_top] {
        if y <= content.max.y {
            painter.hline(content.min.x..=content.max.x, y, line);
        }
    }
    if eq_t > 0.001 {
        painter.hline(content.min.x..=content.max.x, eq_rect.min.y, line);
    }

    let mut step = transport.ui(ui, band, player, alpha, interactive, accent, s);
    // Keyboard media buttons map onto the transport buttons (only
    // while our window is focused — VK_MEDIA_* are global-visible).
    if ui
        .ctx()
        .input(|i| i.viewport().focused.unwrap_or(false))
    {
        media_key_events(|k| match k {
            MediaKey::PlayPause => {
                if let Some(p) = player {
                    p.toggle();
                }
            }
            MediaKey::Stop => {
                if let Some(p) = player {
                    p.stop();
                }
            }
            MediaKey::Next => step = transport::Step::Next,
            MediaKey::Prev => step = transport::Step::Prev,
        });
    }
    // Ctrl+R cycles the repeat mode (ToggleRepeat).
    if interactive
        && !ui.ctx().text_edit_focused()
        && ui
            .ctx()
            .input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::R))
    {
        transport.repeat = match transport.repeat {
            transport::RepeatMode::Off => transport::RepeatMode::All,
            transport::RepeatMode::All => transport::RepeatMode::One,
            transport::RepeatMode::One => transport::RepeatMode::Off,
        };
    }
    // The EQ drawer under the (lifted) transport band — input unlocks
    // once the reveal finishes, like the album strip's gating.
    if eq_t > 0.001 {
        if equalizer::panel(
            ui,
            eq_rect,
            &mut eq.bands,
            alpha,
            interactive && eq_t > 0.95,
            s,
        ) {
            if let Some(p) = player {
                for (i, &db) in eq.bands.iter().enumerate() {
                    p.set_band_gain(i, db);
                }
            }
        }
    }

    // Expanding search pill docked top-right (MusicView's AlbumSearch,
    // Margin 0,15,30,0) — filters the cover view when it exists.
    search.ui(
        ui,
        content.max.x - 30.0 * s,
        content.min.y + 26.0 * s,
        "Search",
        true,
        false,
        alpha,
        interactive,
        s,
    );
    // The band's search slot is the album filter itself — a bigger
    // expanding pill that opens rightward from the magnifier instead
    // of a toggle button (filters `shown` like the old strip pill).
    album_search.ui_band(
        ui,
        transport.search_icon,
        "Filter Albums",
        true,
        false,
        alpha,
        interactive,
        s,
    );

    let bar_rect = Rect::from_min_max(
        Pos2::new(content.min.x + s, menu_top - 10.0 * s - WAVEFORM_BAR_H * s),
        Pos2::new(content.max.x - s, menu_top - 10.0 * s),
    );
    // Now-playing track info, top-left (MusicView's Metadata
    // StackPanel, Margin 25,20 — fades in with HasCurrentMediaLoaded).
    let meta_t = ui.ctx().animate_bool_with_time(
        ui.id().with("trackinfo"),
        playing_track.is_some(),
        0.6,
    );
    if meta_t > 0.001 {
        if let Some(tp) = playing_track {
            let (title, artist, album) = carousel.display_for(tp);
            let x = content.min.x + 25.0 * s + 2.0 * s;
            let mut y = content.min.y + 20.0 * s;
            let paint = ui.painter();
            let a = alpha * meta_t;
            paint.text(
                Pos2::new(x, y),
                Align2::LEFT_TOP,
                title,
                FontId::proportional(28.0 * s),
                Color32::WHITE.gamma_multiply(a),
            );
            y += 37.0 * s;
            paint.text(
                Pos2::new(x, y),
                Align2::LEFT_TOP,
                artist,
                FontId::proportional(22.0 * s),
                Color32::from_rgb(0xCC, 0xCC, 0xCC).gamma_multiply(a),
            );
            y += 29.0 * s;
            paint.text(
                Pos2::new(x, y),
                Align2::LEFT_TOP,
                album,
                FontId::proportional(22.0 * s),
                Color32::from_rgb(0x88, 0x88, 0x88).gamma_multiply(a),
            );
            y += 42.0 * s;
            let (position, duration) = player
                .map(|p| (p.position().as_secs_f64(), p.duration().as_secs_f64()))
                .unwrap_or((0.0, 0.0));
            paint.text(
                Pos2::new(x, y),
                Align2::LEFT_TOP,
                format!("{} / {}", fmt_time(position), fmt_time(duration)),
                FontId::proportional(33.0 * s),
                Color32::WHITE.gamma_multiply(a),
            );
        }
    }
    // Spectrum: bottom of the content row, right above the transport
    // band (MusicView's GlSpectrumControl, Height=SpectrumHeight,
    // VerticalAlignment=Bottom, IsHitTestVisible=False). Cover-derived
    // colors override the configured gradient when enabled and a cover
    // was extracted (GetDominantColorGradient). Hidden in the
    // horizontal grid layout (Lacrima's IsMusicSpectrumVisible).
    if spec_cfg.show && cover_mode != carousel::Mode::HorizontalGrid {
        let manual = spec_cfg
            .gradient
            .map(|c| Color32::from_rgb(c[0], c[1], c[2]));
        let colors: &[Color32] = if spec_cfg.cover_colors {
            cover_colors.unwrap_or(&manual)
        } else {
            &manual
        };
        let spec_h = (spec_cfg.height * s).min(upper.height());
        let spec_rect = Rect::from_min_max(
            Pos2::new(content.min.x, upper.max.y - spec_h),
            upper.max,
        );
        spectrum.paint(&painter, spec_rect, spec_cfg, colors, alpha);
    }
    let (position, duration) = player
        .map(|p| (p.position().as_secs_f64(), p.duration().as_secs_f64()))
        .unwrap_or((0.0, 0.0));
    let mut seek = bar.ui(
        ui,
        bar_rect,
        waveform,
        position,
        duration,
        waveform_loading,
        alpha,
        interactive,
        s,
    );
    // Playhead ticks need frames even with no input events.
    if matches!(player.map(|p| p.state()), Some(audio::PlayerState::Playing)) {
        ui.ctx().request_repaint();
    }
    // While collapsing, the strip slides down out of the shrinking
    // band (the band itself clips at the window edge). Search filters
    // the shown albums like MusicListView's Filter Albums box.
    let mut filtered: Vec<library::Album>;
    let shown: &mut Vec<library::Album> = if album_search.text.trim().is_empty() {
        albums
    } else {
        let q = album_search.text.trim().to_lowercase();
        filtered = albums
            .iter()
            .filter(|a| a.title.to_lowercase().contains(&q))
            .cloned()
            .collect();
        &mut filtered
    };
    let action = if list_t > 0.001 {
        let slide = row_rect.translate(Vec2::new(
            0.0,
            (1.0 - list_t) * (content.max.y - row_rect.min.y),
        ));
        row.ui(
            ui,
            slide,
            shown,
            alpha * list_t,
            interactive && list_t > 0.999,
            s,
            scanning,
            row_reorder_armed,
        )
    } else {
        album_row::Action::None
    };
    let mut edit = None;
    let mut prompt = None;
    let mut delete: Option<Vec<std::path::PathBuf>> = None;
    let mut clear = false;
    let mut export = false;
    let mut play = match carousel_action {
        carousel::Action::Play(path) => Some(path),
        carousel::Action::Edit(path) => {
            edit = Some(path);
            None
        }
        carousel::Action::AddUrl => {
            prompt = Some(url_prompt::Kind::Url);
            None
        }
        carousel::Action::AddPlaylist => {
            prompt = Some(url_prompt::Kind::Playlist);
            None
        }
        carousel::Action::Delete(paths) => {
            delete = Some(paths);
            None
        }
        carousel::Action::Clear => {
            clear = true;
            None
        }
        carousel::Action::Export => {
            export = true;
            None
        }
        // ROM-mode actions never fire on the music carousel.
        carousel::Action::AddRoms
        | carousel::Action::ScanFolder
        | carousel::Action::Patches(_)
        | carousel::Action::CustomConfig(_) => None,
        carousel::Action::None => None,
    };
    // Transport prev/next resolve against the loaded album's items
    // (Previous/NextMedia): prev restarts the track once it's past 3s,
    // otherwise walks back one item; next walks forward. RepeatAll
    // wraps at the album edges.
    let wrap = transport.repeat == transport::RepeatMode::All;
    match step {
        transport::Step::Prev => {
            if let Some(cur) = playing_track {
                if player
                    .map(|p| p.position().as_secs_f64() > 3.0)
                    .unwrap_or(false)
                {
                    if seek.is_none() {
                        seek = Some(0.0);
                    }
                } else {
                    play = carousel.step_path(cur, -1, wrap).or(play);
                }
            }
        }
        transport::Step::Next => {
            if let Some(cur) = playing_track {
                play = carousel.step_path(cur, 1, wrap).or(play);
            }
        }
        transport::Step::None => {}
    }
    (action, seek, play, edit, prompt, delete, clear, export)
}

/// Drop an online item's on-disk caches — stream ranges, resolved
/// metadata, and thumbnail bytes all key off the item url, so a
/// deleted item's data would be orphaned otherwise. Shared between
/// the item Delete, Clear Album, and Delete Album paths.
fn evict_url_caches(url: &str) {
    audio::stream_cache::delete(url);
    meta_cache::remove(url);
    // Cover caches key per thumbnail url — the item's own resolved
    // thumb went with `remove`; the guessed img.youtube.com
    // candidates each may hold an .img too.
    if let Some(id) = ytdlp::extract_video_id(url) {
        for cand in ytdlp::thumbnail_candidates(&id) {
            meta_cache::remove_image(&cand);
        }
    }
}

/// Persist the loaded album's URL items into Settings.Library.UrlItems
/// (Lacrima serializes url children with the album snapshot) and keep
/// the tile's export gating in sync.
fn persist_url_items(
    carousel: &carousel::Carousel,
    albums: &mut Vec<library::Album>,
    settings: &mut settings::Settings,
) {
    let Some(key) = carousel.album_key().map(|k| k.to_string()) else {
        return;
    };
    let urls = carousel.url_items();
    if let Some(a) = albums
        .iter_mut()
        .find(|a| a.dir.to_string_lossy() == key)
    {
        // Rebuilds the tile's web fan layers too — mirroring the
        // loaded items' covers (resolved thumbs where they landed),
        // so adds/removes/order changes propagate to the tile.
        let covers = carousel.url_covers();
        a.sync_url_covers(covers, urls.len());
    }
    settings.library.url_items.insert(key, urls);
    settings::save(settings);
}

/// "m:ss" / "h:mm:ss" (the waveform bar's TimeSpanConverter format).
fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// CubicEaseInOut (Lacrima's transition easing).
fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn placeholder_view(ui: &mut egui::Ui, content: Rect, name: &str, s: f32) {
    ui.painter().text(
        content.center(),
        Align2::CENTER_CENTER,
        name,
        FontId::proportional(60.0 * s),
        Color32::from_gray(170),
    );
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

const SETTINGS_TABS: [&str; 6] = [
    "UI",
    "Emulation",
    "Updates",
    "Audio",
    "Carousel",
    "Libraries",
];

/// Lacrima's `SettingsOverlay`: a dark panel that slides in from the right
/// edge of the content area, with a tab strip and the app version pinned
/// to the bottom-right.
fn settings_overlay(
    ui: &mut egui::Ui,
    content: Rect,
    t: f32,
    radius: u8,
    tab: &mut usize,
    open: &mut bool,
    settings: &mut settings::Settings,
    transport: &mut transport::Transport,
    ytdlp: &mut ytdlp::YtDlp,
    emu: &mut emulation::EmuView,
    toy_names: &[String],
    scale: f32,
) {
    let w = (content.width() * 0.38).clamp(240.0, content.width());
    let panel = Rect::from_min_max(
        Pos2::new(content.max.x - w * t, content.min.y),
        content.max,
    );
    let painter = ui.painter().clone();

    // Clicking the exposed content left of the panel closes it; the panel
    // itself swallows clicks so they don't reach the view underneath.
    let outside = Rect::from_min_max(content.min, Pos2::new(panel.min.x, content.max.y));
    if ui
        .interact(outside, ui.id().with("settings-outside"), Sense::click())
        .clicked()
    {
        *open = false;
    }
    ui.interact(panel, ui.id().with("settings-panel"), Sense::click());

    painter.rect_filled(
        panel,
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: 0,
            se: radius,
        },
        Color32::from_black_alpha((242.0 * t) as u8),
    );
    painter.line_segment(
        [panel.left_top(), panel.left_bottom()],
        Stroke::new(1.0, Color32::from_gray(50).gamma_multiply(t)),
    );

    // Tab strip across the top (75px header band, tabs FontSize 30);
    // selected tab is underlined.
    let mut x = panel.min.x + 15.0 * scale;
    let y = panel.min.y + 14.0 * scale;
    let font = FontId::proportional(30.0 * scale);
    for (i, name) in SETTINGS_TABS.iter().enumerate() {
        let text_w = painter
            .layout_no_wrap(name.to_string(), font.clone(), Color32::WHITE)
            .rect
            .width();
        let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(text_w, 44.0 * scale));
        let resp = ui.interact(rect, ui.id().with(("settings-tab", i)), Sense::click());
        let color = if i == *tab {
            Color32::WHITE
        } else if resp.hovered() {
            Color32::from_gray(200)
        } else {
            Color32::from_gray(140)
        };
        painter.text(
            rect.min,
            Align2::LEFT_TOP,
            *name,
            font.clone(),
            color.gamma_multiply(t),
        );
        if i == *tab {
            painter.line_segment(
                [
                    Pos2::new(x, rect.max.y - 2.0 * scale),
                    Pos2::new(x + text_w, rect.max.y - 2.0 * scale),
                ],
                Stroke::new(2.0 * scale, Color32::WHITE.gamma_multiply(t)),
            );
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.clicked() {
            *tab = i;
        }
        x += text_w + 28.0 * scale;
    }

    // Body content per tab.
    if *tab == 0 {
        ui_tab_effects(ui, panel, t, settings, toy_names, scale);
    } else if *tab == 1 {
        emulation::settings_tab(ui, panel, t, emu, &mut settings.emulation, scale);
    } else if *tab == 3 {
        ui_tab_audio(ui, panel, t, settings, scale);
    } else if *tab == 4 {
        ui_tab_carousel(ui, panel, t, settings, transport, scale);
    } else if *tab == 5 {
        ui_tab_libraries(ui, panel, t, ytdlp, scale);
    } else {
        painter.text(
            panel.center(),
            Align2::CENTER_CENTER,
            format!("{} settings", SETTINGS_TABS[*tab]),
            FontId::proportional(30.0 * scale),
            Color32::from_gray(110).gamma_multiply(t),
        );
    }

    // Footer row (Margin 15,8,12,6, top border): separator above the
    // version block pinned right, "Open AES Directory" button left.
    painter.line_segment(
        [
            Pos2::new(panel.min.x + 15.0 * scale, panel.max.y - 70.0 * scale),
            Pos2::new(panel.max.x - 12.0 * scale, panel.max.y - 70.0 * scale),
        ],
        Stroke::new(1.0, Color32::from_gray(45).gamma_multiply(t)),
    );

    // "Open AES Directory" — reveals the app's data root in the OS
    // file manager (settings, caches, tools, shaders all live there).
    let btn_rect = Rect::from_min_size(
        Pos2::new(panel.min.x + 15.0 * scale, panel.max.y - 60.0 * scale),
        Vec2::new(320.0 * scale, 48.0 * scale),
    );
    let btn = ui.interact(btn_rect, ui.id().with("open-aes-dir"), Sense::click());
    let hot = btn.hovered();
    painter.rect_filled(
        btn_rect,
        CornerRadius::same((10.0 * scale).round() as u8),
        Color32::from_white_alpha((if hot { 18.0 } else { 8.0 } * t) as u8),
    );
    painter.rect_stroke(
        btn_rect,
        CornerRadius::same((10.0 * scale).round() as u8),
        Stroke::new(
            1.0,
            Color32::from_white_alpha(
                ((if hot { 110.0 } else { 70.0 }) * t) as u8,
            ),
        ),
        StrokeKind::Inside,
    );
    painter.text(
        btn_rect.center(),
        Align2::CENTER_CENTER,
        "Open AES Directory",
        FontId::proportional(20.0 * scale),
        Color32::from_gray(if hot { 235 } else { 200 }).gamma_multiply(t),
    );
    if hot {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if btn.clicked() {
        if let Some(dir) = settings::data_root() {
            let _ = if cfg!(windows) {
                std::process::Command::new("explorer").arg(&dir).spawn()
            } else if cfg!(target_os = "macos") {
                std::process::Command::new("open").arg(&dir).spawn()
            } else {
                std::process::Command::new("xdg-open").arg(&dir).spawn()
            };
        }
    }

    painter.text(
        Pos2::new(panel.max.x - 24.0 * scale, panel.max.y - 46.0 * scale),
        Align2::RIGHT_CENTER,
        "CURRENT VERSION",
        FontId::proportional(12.0 * scale),
        Color32::from_gray(110).gamma_multiply(t),
    );
    painter.text(
        Pos2::new(panel.max.x - 24.0 * scale, panel.max.y - 26.0 * scale),
        Align2::RIGHT_CENTER,
        env!("CARGO_PKG_VERSION"),
        FontId::proportional(16.0 * scale),
        Color32::from_gray(200).gamma_multiply(t),
    );
}

/// UI tab content: the "Scaling" slider (Lacrima's ScaleFactor), then
/// the "Effects" section with the Show Lacrima toggle and count slider.
fn ui_tab_effects(
    ui: &mut egui::Ui,
    panel: Rect,
    t: f32,
    settings: &mut settings::Settings,
    toy_names: &[String],
    s: f32,
) {
    let painter = ui.painter().clone();
    // TabControl margin-left 15 + StackPanel margin 15,20 (75px header).
    let x = panel.min.x + 30.0 * s;
    let right = panel.max.x - 30.0 * s;
    let mut y = panel.min.y + 95.0 * s;
    let label = FontId::proportional(20.0 * s);
    let text = |painter: &egui::Painter, y: f32, s: &str| {
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_TOP,
            s,
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
    };

    // "Scaling" — header FontSize 30, then PillProgressSlider
    // 260x28 / track 16 with the "0.00X" readout (FontSize 20, margin 8).
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Scaling",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 46.0 * s;
    let scale_track =
        Rect::from_min_size(Pos2::new(x, y + 6.0 * s), Vec2::new(260.0 * s, 16.0 * s));
    let mut factor = settings.ui.scale_factor;
    if pill_slider(ui, scale_track, "ui-scale", &mut factor, 1.0, 3.0, 0.01, t, None) {
        settings.ui.scale_factor = factor;
    }
    painter.text(
        Pos2::new(scale_track.max.x + 8.0 * s, scale_track.center().y),
        Align2::LEFT_CENTER,
        format!("{:.2}X", settings.ui.scale_factor),
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    y += 43.0 * s;

    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Effects",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 46.0 * s;
    painter.line_segment(
        [Pos2::new(x, y), Pos2::new(right, y)],
        Stroke::new(1.0, Color32::from_gray(55).gamma_multiply(t)),
    );
    y += 15.0 * s;

    // Toggle rows: FontSize 20 label, switch at +8 margin, 15px row gap.
    let label_row = |ui: &mut egui::Ui, y: f32, name: &str, on: &mut bool, salt: &str| {
        let w = painter
            .layout_no_wrap(name.to_owned(), label.clone(), Color32::WHITE)
            .rect
            .width();
        text(&painter, y, name);
        let sw = Rect::from_min_size(
            Pos2::new(x + w + 8.0 * s, y - 2.0 * s),
            Vec2::new(44.0 * s, 22.0 * s),
        );
        toggle_switch(ui, sw, salt, on, t);
    };
    label_row(ui, y, "Show Lacrima", &mut settings.effects.show_lacrima, "lacrima-toggle");
    y += 40.0 * s;
    label_row(
        ui,
        y,
        "Lacrima cover color",
        &mut settings.effects.lacrima_cover_color,
        "lacrima-cover-toggle",
    );
    y += 40.0 * s;

    // Lacrima count: pill slider + "N / 60000" readout.
    text(&painter, y, "Lacrima count");
    y += 30.0 * s;
    let track = Rect::from_min_size(Pos2::new(x, y), Vec2::new(260.0 * s, 16.0 * s));
    let mut count = settings.effects.lacrima_count as f32;
    if pill_slider(
        ui,
        track,
        "lacrima-count",
        &mut count,
        particles::MIN_COUNT as f32,
        particles::MAX_COUNT as f32,
        1.0,
        t,
        None,
    ) {
        settings.effects.lacrima_count = count as u32;
    }
    painter.text(
        Pos2::new(track.max.x + 8.0 * s, track.center().y),
        Align2::LEFT_CENTER,
        format!("{} / {}", settings.effects.lacrima_count, particles::MAX_COUNT),
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    y += 43.0 * s;
    label_row(
        ui,
        y,
        "Show Edge Border",
        &mut settings.effects.show_edge_border,
        "edge-border-toggle",
    );
    y += 40.0 * s;
    label_row(
        ui,
        y,
        "Animate title",
        &mut settings.effects.animate_title,
        "animate-title-toggle",
    );
    y += 46.0 * s;

    // "ShaderToy" section — Show toggle + "Current" ComboBox listing
    // every compiled shader (SettingsOverlay.axaml's ShaderToy block).
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "ShaderToy",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 46.0 * s;
    painter.line_segment(
        [Pos2::new(x, y), Pos2::new(right, y)],
        Stroke::new(1.0, Color32::from_gray(55).gamma_multiply(t)),
    );
    y += 15.0 * s;
    label_row(
        ui,
        y,
        "Show ShaderToy",
        &mut settings.effects.show_shadertoy,
        "shadertoy-toggle",
    );
    y += 45.0 * s;

    text(&painter, y, "Current");
    let w = painter
        .layout_no_wrap("Current".to_owned(), label.clone(), Color32::WHITE)
        .rect
        .width();
    let combo_rect = Rect::from_min_size(
        Pos2::new(x + w + 10.0 * s, y - 4.0 * s),
        Vec2::new(250.0 * s, 30.0 * s),
    );
    let mut cui = ui.new_child(
        UiBuilder::new()
            .max_rect(combo_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    cui.set_opacity(t);
    egui::ComboBox::from_id_salt("shadertoy-pick")
        .selected_text(settings.effects.shadertoy.clone())
        .width(250.0 * s)
        .show_ui(&mut cui, |ui| {
            for name in toy_names {
                ui.selectable_value(
                    &mut settings.effects.shadertoy,
                    name.clone(),
                    name.as_str(),
                );
            }
        });
}

/// Audio tab content: Lacrima's "Spectrum" section — height/width/
/// spacing sliders, show + cover-colors toggles, and the five gradient
/// color swatches — then the "ReplayGain / Normalization" and
/// "Playback Options" sections (SettingsOverlay.axaml's Audio tab).
fn ui_tab_audio(
    ui: &mut egui::Ui,
    panel: Rect,
    t: f32,
    settings: &mut settings::Settings,
    s: f32,
) {
    // The tab grew past the panel height — it scrolls like Lacrima's
    // ScrollViewer. Content is painted into a body rect clipped below
    // the tab strip and above the footer; the clip also makes scrolled-
    // away widgets non-interactive (egui intersects hit rects with it).
    // Clip ends just above the footer separator (-70s) — content must
    // never underlap the "Open AES Directory" row.
    let body = Rect::from_min_max(
        Pos2::new(panel.min.x, panel.min.y + 78.0 * s),
        Pos2::new(panel.max.x, panel.max.y - 74.0 * s),
    );
    let scroll_id = ui.id().with("audio-scroll");
    let mut scroll = ui
        .ctx()
        .data_mut(|d| d.get_persisted::<f32>(scroll_id).unwrap_or(0.0));
    let over_body = ui
        .ctx()
        .input(|i| i.pointer.hover_pos().is_some_and(|p| body.contains(p)));
    if over_body {
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
    let mut y = panel.min.y + 95.0 * s - scroll;
    let label = FontId::proportional(20.0 * s);
    let spec = &mut settings.spectrum;

    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Spectrum",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 56.0 * s;

    // Slider rows (Lacrima's 3-row grid): label, 300px PillProgressSlider,
    // value readout.
    let slider_row = |ui: &mut egui::Ui,
                          y: f32,
                          name: &str,
                          value: &mut f32,
                          min: f32,
                          max: f32,
                          step: f32,
                          decimals: usize| {
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_CENTER,
            name,
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
        let track = Rect::from_min_size(
            Pos2::new(x + 130.0 * s, y - 8.0 * s),
            Vec2::new(300.0 * s, 16.0 * s),
        );
        pill_slider(ui, track, &format!("spec-{name}"), value, min, max, step, t, None);
        let text = match decimals {
            0 => format!("{value:.0}"),
            _ => {
                let v = format!("{value:.2}");
                v.trim_end_matches('0').trim_end_matches('.').to_owned()
            }
        };
        painter.text(
            Pos2::new(track.max.x + 10.0 * s, y),
            Align2::LEFT_CENTER,
            text,
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
    };
    slider_row(&mut cui, y, "Height", &mut spec.height, 50.0, 1000.0, 1.0, 0);
    y += 44.0 * s;
    slider_row(&mut cui, y, "Bar Width", &mut spec.bar_width, 1.0, 100.0, 1.0, 2);
    y += 44.0 * s;
    slider_row(&mut cui, y, "Bar Spacing", &mut spec.bar_spacing, 0.0, 100.0, 1.0, 2);
    y += 44.0 * s;

    // Toggle rows, same styling as the Effects tab.
    let toggle_row = |ui: &mut egui::Ui, y: f32, name: &str, on: &mut bool, salt: &str| {
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_CENTER,
            name,
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
        let w = painter
            .layout_no_wrap(name.to_owned(), label.clone(), Color32::WHITE)
            .rect
            .width();
        let sw = Rect::from_min_size(
            Pos2::new(x + w + 8.0 * s, y - 11.0 * s),
            Vec2::new(44.0 * s, 22.0 * s),
        );
        toggle_switch(ui, sw, salt, on, t);
    };
    toggle_row(&mut cui, y, "Show Spectrum", &mut spec.show, "spec-show");
    y += 40.0 * s;
    toggle_row(&mut cui, y, "Use Cover Colors", &mut spec.cover_colors, "spec-cover");
    y += 48.0 * s;

    // Gradient swatches: 80x40 ColorPicker equivalents in a wrap row.
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "Spectrum Gradient",
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    y += 32.0 * s;
    for (i, rgb) in spec.gradient.iter_mut().enumerate() {
        let rect = Rect::from_min_size(
            Pos2::new(x + i as f32 * 90.0 * s, y),
            Vec2::new(80.0 * s, 40.0 * s),
        );
        let mut c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
        cui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            ui.centered_and_justified(|ui| {
                if ui.color_edit_button_srgba(&mut c).changed() {
                    *rgb = [c.r(), c.g(), c.b()];
                }
            });
        });
    }
    y += 40.0 * s + 56.0 * s;

    // Volume knob accent — same ColorPicker swatch as the gradient
    // stops, bound to Audio.VolumeKnobColor.
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "Volume Knob",
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    {
        let rect = Rect::from_min_size(
            Pos2::new(x + 130.0 * s, y - 20.0 * s),
            Vec2::new(80.0 * s, 40.0 * s),
        );
        let rgb = &mut settings.audio.volume_knob_color;
        let mut c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
        cui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            ui.centered_and_justified(|ui| {
                if ui.color_edit_button_srgba(&mut c).changed() {
                    *rgb = [c.r(), c.g(), c.b()];
                }
            });
        });
    }
    y += 56.0 * s;

    // ---- "ReplayGain / Normalization" (SettingsOverlay.axaml) ------
    let audio = &mut settings.audio;
    let sub = FontId::proportional(22.0 * s);
    let cb_font = 16.0; // checkbox labels (Lacrima's FontSize 16 rows)
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "ReplayGain / Normalization",
        sub.clone(),
        Color32::from_gray(235).gamma_multiply(t),
    );
    y += 52.0 * s;

    check_box(
        &mut cui,
        x,
        y,
        "Smooth volume changing",
        cb_font,
        &mut audio.smooth_volume_change,
        t,
        s,
        "audio-smooth",
    );
    y += 38.0 * s;
    check_box(
        &mut cui,
        x,
        y,
        "Logarithmic volume control",
        cb_font,
        &mut audio.logarithmic_volume_control,
        t,
        s,
        "audio-logvol",
    );
    y += 38.0 * s;
    check_box(
        &mut cui,
        x,
        y,
        "Loudness compensated volume control",
        cb_font,
        &mut audio.loudness_compensated_volume,
        t,
        s,
        "audio-loudness",
    );
    y += 48.0 * s;

    // "Enable ReplayGain" + ToggleSwitch (Lacrima FontSize 16 row).
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "Enable ReplayGain",
        FontId::proportional(17.0 * s),
        Color32::from_gray(235).gamma_multiply(t),
    );
    let sw = Rect::from_min_size(
        Pos2::new(x + 150.0 * s, y - 11.0 * s),
        Vec2::new(44.0 * s, 22.0 * s),
    );
    toggle_switch(&mut cui, sw, "audio-rg", &mut audio.replay_gain_enabled, t);
    // Avalonia ToggleSwitch renders its On/Off content next to the pill.
    painter.text(
        Pos2::new(sw.max.x + 8.0 * s, y),
        Align2::LEFT_CENTER,
        if audio.replay_gain_enabled { "On" } else { "Off" },
        FontId::proportional(14.0 * s),
        Color32::from_gray(180).gamma_multiply(t),
    );
    y += 44.0 * s;

    // "Analyse file on-the-fly" + its preamp slider (220px, ±12 dB).
    check_box(
        &mut cui,
        x,
        y,
        "Analyse file on-the-fly",
        cb_font,
        &mut audio.replay_gain_analyze_on_the_fly,
        t,
        s,
        "audio-rg-analyze",
    );
    y += 42.0 * s;
    painter.text(
        Pos2::new(x + 10.0 * s, y),
        Align2::LEFT_CENTER,
        "Preamp (dB):",
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    let track = Rect::from_min_size(
        Pos2::new(x + 150.0 * s, y - 8.0 * s),
        Vec2::new(220.0 * s, 16.0 * s),
    );
    pill_slider(
        &mut cui,
        track,
        "audio-rg-preamp",
        &mut audio.replay_gain_preamp_db,
        -12.0,
        12.0,
        0.1,
        t,
        None,
    );
    painter.text(
        Pos2::new(track.max.x + 8.0 * s, y),
        Align2::LEFT_CENTER,
        format!("{} dB", trim_dec(audio.replay_gain_preamp_db)),
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    y += 48.0 * s;

    // "Use value from tags" + its preamp slider (180px) + the
    // Track/Album source ComboBox (Lacrima's ReplayGainTagSource).
    check_box(
        &mut cui,
        x,
        y,
        "Use value from tags",
        cb_font,
        &mut audio.replay_gain_use_tags,
        t,
        s,
        "audio-rg-tags",
    );
    y += 42.0 * s;
    painter.text(
        Pos2::new(x + 10.0 * s, y),
        Align2::LEFT_CENTER,
        "Preamp (dB):",
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    let track = Rect::from_min_size(
        Pos2::new(x + 150.0 * s, y - 8.0 * s),
        Vec2::new(180.0 * s, 16.0 * s),
    );
    pill_slider(
        &mut cui,
        track,
        "audio-rg-tags-preamp",
        &mut audio.replay_gain_tags_preamp_db,
        -12.0,
        12.0,
        0.1,
        t,
        None,
    );
    painter.text(
        Pos2::new(track.max.x + 8.0 * s, y),
        Align2::LEFT_CENTER,
        format!("{} dB", trim_dec(audio.replay_gain_tags_preamp_db)),
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    // The Track/Album ComboBox sits right of the slider like Lacrima's
    // WrapPanel row — wraps under it when the panel is too narrow.
    let right = panel.max.x - 30.0 * s;
    let combo_pos = if track.max.x + 70.0 * s + 110.0 * s <= right {
        Pos2::new(track.max.x + 70.0 * s, y - 15.0 * s)
    } else {
        y += 40.0 * s;
        Pos2::new(x + 150.0 * s, y - 15.0 * s)
    };
    let combo_rect = Rect::from_min_size(combo_pos, Vec2::new(110.0 * s, 30.0 * s));
    let mut ccui = cui.new_child(
        UiBuilder::new()
            .max_rect(combo_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    ccui.set_opacity(t);
    egui::ComboBox::from_id_salt("audio-rg-source")
        .selected_text(if audio.replay_gain_tag_source == 1 {
            "Album"
        } else {
            "Track"
        })
        .width(110.0 * s)
        .show_ui(&mut ccui, |ui| {
            ui.selectable_value(&mut audio.replay_gain_tag_source, 0u8, "Track");
            ui.selectable_value(&mut audio.replay_gain_tag_source, 1u8, "Album");
        });
    y += 54.0 * s;

    // ---- "Playback Options" (SettingsOverlay.axaml) -----------------
    painter.text(
        Pos2::new(x, y + 26.0 * s),
        Align2::LEFT_TOP,
        "Playback Options",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 80.0 * s;

    check_box(
        &mut cui,
        x,
        y,
        "Restart time frame",
        16.0,
        &mut audio.restart_on_resume_enabled,
        t,
        s,
        "audio-restart",
    );
    y += 44.0 * s;

    // The timeframe row dims out when the checkbox is off (Lacrima's
    // IsEnabled binding) — the child ui's disabled flag makes the
    // slider inert, the reduced fade dims the paint.
    let restart_on = audio.restart_on_resume_enabled;
    let dim = if restart_on { t } else { t * 0.4 };
    painter.text(
        Pos2::new(x + 20.0 * s, y),
        Align2::LEFT_CENTER,
        "0 to (s)",
        FontId::proportional(16.0 * s),
        Color32::from_gray(225).gamma_multiply(dim),
    );
    let track = Rect::from_min_size(
        Pos2::new(x + 90.0 * s, y - 8.0 * s),
        Vec2::new(220.0 * s, 16.0 * s),
    );
    {
        let mut dui_b = UiBuilder::new().max_rect(body);
        if !restart_on {
            dui_b = dui_b.disabled();
        }
        let mut dui = cui.new_child(dui_b);
        dui.set_clip_rect(body);
        pill_slider(
            &mut dui,
            track,
            "audio-restart-secs",
            &mut audio.restart_on_resume_threshold_seconds,
            0.0,
            20.0,
            0.5,
            dim,
            None,
        );
    }
    painter.text(
        Pos2::new(track.max.x + 10.0 * s, y),
        Align2::LEFT_CENTER,
        format!("{:.1} s", audio.restart_on_resume_threshold_seconds),
        label.clone(),
        Color32::from_gray(225).gamma_multiply(dim),
    );
    y += 36.0 * s;
    painter.text(
        Pos2::new(x + 20.0 * s, y),
        Align2::LEFT_TOP,
        "Restart from the beginning when resuming after a pause within this timeframe.",
        FontId::proportional(13.0 * s),
        Color32::from_rgb(0xBB, 0xBB, 0xBB).gamma_multiply(dim),
    );
    y += 46.0 * s;

    check_box(
        &mut cui,
        x,
        y,
        "Pause when volume is set to zero",
        16.0,
        &mut audio.pause_when_volume_is_zero,
        t,
        s,
        "audio-pause-zero",
    );
    y += 46.0 * s;

    // "Silence advance delay (ms):" — Lacrima's digits-only 80px
    // TextBox (it sits under Progress Bar in the overlay; our tab has
    // no progress-bar section so it lives in Playback Options).
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "Silence advance delay (ms):",
        FontId::proportional(16.0 * s),
        Color32::from_gray(225).gamma_multiply(t),
    );
    let text_id = ui.id().with("audio-silence-ms");
    let mut text = ui.ctx().data_mut(|d| {
        d.get_persisted::<String>(text_id)
            .unwrap_or_else(|| audio.silence_advance_delay_ms.to_string())
    });
    let box_rect = Rect::from_min_size(
        Pos2::new(x + 210.0 * s, y - 14.0 * s),
        Vec2::new(80.0 * s, 28.0 * s),
    );
    let mut tui = cui.new_child(
        UiBuilder::new()
            .max_rect(box_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    tui.set_opacity(t);
    let r = tui.add(
        egui::TextEdit::singleline(&mut text)
            .desired_width(80.0 * s)
            .font(FontId::proportional(15.0 * s)),
    );
    // Lacrima's TextBox look: a rounded outline around the field.
    painter.rect_stroke(
        box_rect,
        CornerRadius::same(4),
        Stroke::new(
            1.0,
            Color32::from_gray(150).gamma_multiply(t),
        ),
        StrokeKind::Outside,
    );
    if r.changed() {
        // DigitOnlyBehavior — non-digits never stick.
        text.retain(|c| c.is_ascii_digit());
        if let Ok(v) = text.parse::<u32>() {
            audio.silence_advance_delay_ms = v.min(60_000);
        }
        ui.ctx().data_mut(|d| d.insert_persisted(text_id, text));
    }
    y += 46.0 * s;

    // Rustified-only option (no Lacrima equivalent): spectral-
    // subtraction denoiser for AI-generated tracks.
    check_box(
        &mut cui,
        x,
        y,
        "Digital noise reduction (AI music)",
        16.0,
        &mut audio.ai_noise_reduction,
        t,
        s,
        "audio-denoise",
    );
    y += 30.0 * s;
    painter.text(
        Pos2::new(x + 20.0 * s, y),
        Align2::LEFT_TOP,
        "Spectral-gates the hiss/shimmer neural codecs leave in AI-generated tracks.",
        FontId::proportional(13.0 * s),
        Color32::from_rgb(0xBB, 0xBB, 0xBB).gamma_multiply(t),
    );
    y += 34.0 * s;

    // Scroll bookkeeping: clamp against the real content height and
    // show a thin scrollbar thumb when it overflows the body.
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

/// Lacrima's trailing-silence measurement (AudioPlayer's waveform
/// analysis): contiguous tail buckets under 5% of the global peak
/// count as silence. Our buckets are normalized (÷ max, ×1.1, 0.01
/// floor) so the raw 5% threshold lands at 0.055. Under one second of
/// tail silence disables the early advance, same as Lacrima.
fn trailing_silence_seconds(waveform: &[f32], duration: f64) -> f64 {
    if waveform.is_empty() || duration <= 0.0 {
        return 0.0;
    }
    let tail = waveform.iter().rev().take_while(|&&v| v < 0.055).count();
    let secs = tail as f64 / waveform.len() as f64 * duration;
    if secs >= 1.0 { secs } else { 0.0 }
}

/// "0.##" readout — trims trailing zeros like Lacrima's StringFormat.
fn trim_dec(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}
/// radio group, then mode-dependent controls: Item Spacing/Scale,
/// Vertical Offset, Side Translation, Stack Spacing and Full Cover Size
/// for the carousel; Card Spacing/Opacity and the grid background for
/// the grid modes.
fn ui_tab_carousel(
    ui: &mut egui::Ui,
    panel: Rect,
    t: f32,
    settings: &mut settings::Settings,
    transport: &mut transport::Transport,
    s: f32,
) {
    let painter = ui.painter().clone();
    let x = panel.min.x + 30.0 * s;
    let mut y = panel.min.y + 95.0 * s;
    let label = FontId::proportional(20.0 * s);
    let car = &mut settings.carousel;

    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "Setup",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 50.0 * s;

    // "View Mode" radio group — writes both the persisted setting and
    // the transport's live layout so both stay in sync.
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "View Mode",
        label.clone(),
        Color32::from_gray(225).gamma_multiply(t),
    );
    let mut rx = x + 170.0 * s;
    for (mode, name) in [
        (0u8, "Carousel"),
        (1u8, "Vertical Grid"),
        (2u8, "Horizontal Grid"),
    ] {
        let w = painter
            .layout_no_wrap(name.to_owned(), label.clone(), Color32::WHITE)
            .rect
            .width();
        let rect = Rect::from_min_size(
            Pos2::new(rx, y - 12.0 * s),
            Vec2::new(w + 26.0 * s, 24.0 * s),
        );
        if radio_button(ui, rect, name, car.layout_mode == mode, t, mode) {
            car.layout_mode = mode;
            transport.cover_layout = mode;
        }
        rx += rect.width() + 14.0 * s;
    }
    y += 46.0 * s;

    // Slider rows in the same column layout as the Audio tab.
    let slider_row = |ui: &mut egui::Ui,
                      y: f32,
                      name: &str,
                      value: &mut f32,
                      min: f32,
                      max: f32,
                      step: f32,
                      text: String| {
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_CENTER,
            name,
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
        let track = Rect::from_min_size(
            Pos2::new(x + 170.0 * s, y - 8.0 * s),
            Vec2::new(260.0 * s, 16.0 * s),
        );
        pill_slider(ui, track, &format!("car-{name}"), value, min, max, step, t, None);
        painter.text(
            Pos2::new(track.max.x + 10.0 * s, y),
            Align2::LEFT_CENTER,
            text,
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
    };

    if car.layout_mode == 0 {
        let readout = format!("{:.2}", car.spacing);
        slider_row(ui, y, "Item Spacing", &mut car.spacing, 0.2, 3.0, 0.01, readout);
        y += 44.0 * s;
        let readout = format!("{:.2}X", car.scale);
        slider_row(ui, y, "Item Scale", &mut car.scale, 0.5, 2.0, 0.01, readout);
        y += 44.0 * s;
        let readout = format!("{:.0}", car.vertical_offset);
        slider_row(
            ui,
            y,
            "Vertical Offset",
            &mut car.vertical_offset,
            -400.0,
            400.0,
            1.0,
            readout,
        );
        y += 44.0 * s;
        let readout = format!("{:.0}", car.side_translation);
        slider_row(
            ui,
            y,
            "Side Translation",
            &mut car.side_translation,
            0.0,
            1000.0,
            1.0,
            readout,
        );
        y += 44.0 * s;
        let readout = format!("{:.0}", car.stack_spacing);
        slider_row(
            ui,
            y,
            "Stack Spacing",
            &mut car.stack_spacing,
            0.0,
            400.0,
            1.0,
            readout,
        );
        y += 44.0 * s;

        // "Full Cover Size" toggle row.
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_CENTER,
            "Full Cover Size",
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
        let sw = Rect::from_min_size(
            Pos2::new(x + 170.0 * s, y - 11.0 * s),
            Vec2::new(44.0 * s, 22.0 * s),
        );
        toggle_switch(ui, sw, "car-full-cover", &mut car.use_full_cover_size, t);
    } else {
        // "Cards" section — CardScale (Lacrima binds it to ItemScale;
        // here it's independently tunable), spacing, opacity, and the
        // grid background color.
        painter.text(
            Pos2::new(x, y - 8.0 * s),
            Align2::LEFT_TOP,
            "Cards",
            FontId::proportional(26.0 * s),
            Color32::from_gray(200).gamma_multiply(t),
        );
        y += 34.0 * s;
        let readout = format!("{:.0}px", car.card_size);
        slider_row(
            ui,
            y,
            "Card Size",
            &mut car.card_size,
            120.0,
            600.0,
            1.0,
            readout,
        );
        y += 44.0 * s;
        let readout = format!("{:.0}", car.card_spacing);
        slider_row(
            ui,
            y,
            "Card Spacing",
            &mut car.card_spacing,
            4.0,
            48.0,
            1.0,
            readout,
        );
        y += 44.0 * s;
        let readout = format!("{:.0}%", car.card_opacity * 100.0);
        slider_row(
            ui,
            y,
            "Card Opacity",
            &mut car.card_opacity,
            0.0,
            1.0,
            0.01,
            readout,
        );
        y += 44.0 * s;

        // "Grid Background" color swatch.
        painter.text(
            Pos2::new(x, y),
            Align2::LEFT_CENTER,
            "Grid Background",
            label.clone(),
            Color32::from_gray(225).gamma_multiply(t),
        );
        let rect = Rect::from_min_size(
            Pos2::new(x + 170.0 * s, y - 20.0 * s),
            Vec2::new(80.0 * s, 40.0 * s),
        );
        let rgb = &mut car.grid_bg;
        let mut c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            ui.centered_and_justified(|ui| {
                if ui.color_edit_button_srgba(&mut c).changed() {
                    *rgb = [c.r(), c.g(), c.b()];
                }
            });
        });
    }
}

/// Avalonia-style RadioButton: a ring with a filled inner dot when
/// checked, label to its right. Returns true when clicked.
fn radio_button(
    ui: &mut egui::Ui,
    rect: Rect,
    label: &str,
    checked: bool,
    t: f32,
    salt: u8,
) -> bool {
    let resp = ui.interact(
        rect,
        ui.id().with(("cover-mode-radio", salt)),
        Sense::click(),
    );
    let painter = ui.painter();
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let c = Pos2::new(rect.min.x + 9.0 * rect.height() / 24.0, rect.center().y);
    let r = rect.height() * 0.375;
    let ring = if checked { 235 } else { 150 };
    painter.circle_stroke(
        c,
        r,
        Stroke::new(
            1.5 * rect.height() / 24.0,
            Color32::from_gray(ring).gamma_multiply(t),
        ),
    );
    if checked {
        painter.circle_filled(c, r * 0.55, Color32::WHITE.gamma_multiply(t));
    }
    painter.text(
        Pos2::new(rect.min.x + 24.0 * rect.height() / 24.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(rect.height() * 0.75),
        Color32::from_gray(225).gamma_multiply(t),
    );
    resp.clicked()
}

/// Small Avalonia-style ToggleSwitch: a pill track with a sliding knob,
/// cyan accent when on.
fn toggle_switch(
    ui: &mut egui::Ui,
    rect: Rect,
    salt: &str,
    on: &mut bool,
    fade: f32,
) {
    let resp = ui.interact(rect, ui.id().with(salt), Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let anim = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("v"), *on, 0.12);
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * anim) as u8;
    let painter = ui.painter();
    let radius = CornerRadius::same((rect.height() / 2.0) as u8);
    painter.rect_filled(
        rect,
        radius,
        Color32::from_rgb(lerp(60, 0x12), lerp(60, 0xC0), lerp(60, 0xF0))
            .gamma_multiply(fade),
    );
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, Color32::from_gray(120).gamma_multiply(fade)),
        StrokeKind::Inside,
    );
    let r = rect.height() * 0.5 - 2.0;
    let cx = rect.min.x + 2.0 + r + (rect.width() - 4.0 - 2.0 * r) * anim;
    painter.circle_filled(
        Pos2::new(cx, rect.center().y),
        r,
        Color32::from_gray(235).gamma_multiply(fade),
    );
}

/// Avalonia CheckBox: an 18px rounded box (accent fill + white check
/// when on) followed by its label; the label is part of the hit area.
/// `(x, y)` is the row's left edge at vertical center.
#[allow(clippy::too_many_arguments)]
fn check_box(
    ui: &mut egui::Ui,
    x: f32,
    y: f32,
    label: &str,
    font: f32,
    checked: &mut bool,
    fade: f32,
    s: f32,
    salt: &str,
) -> bool {
    let painter = ui.painter().clone();
    let bs = 18.0 * s;
    let brect = Rect::from_min_size(Pos2::new(x, y - bs * 0.5), Vec2::splat(bs));
    let fid = FontId::proportional(font * s);
    let lw = painter
        .layout_no_wrap(label.to_owned(), fid.clone(), Color32::WHITE)
        .rect
        .width();
    let hit = Rect::from_min_max(
        Pos2::new(brect.min.x - 3.0 * s, brect.min.y - 4.0 * s),
        Pos2::new(brect.max.x + 10.0 * s + lw, brect.max.y + 4.0 * s),
    );
    let resp = ui.interact(hit, ui.id().with(salt), Sense::click());
    let mut changed = false;
    if resp.clicked() {
        *checked = !*checked;
        changed = true;
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let accent = Color32::from_rgb(0x12, 0xC0, 0xF0);
    painter.rect_filled(
        brect,
        CornerRadius::same((4.0 * s) as u8),
        if *checked {
            accent.gamma_multiply(fade)
        } else {
            Color32::from_black_alpha(140).gamma_multiply(fade)
        },
    );
    painter.rect_stroke(
        brect,
        CornerRadius::same((4.0 * s) as u8),
        Stroke::new(
            1.2 * s,
            if *checked {
                accent.gamma_multiply(fade)
            } else {
                Color32::from_gray(if resp.hovered() { 190 } else { 150 })
                    .gamma_multiply(fade)
            },
        ),
        StrokeKind::Inside,
    );
    if *checked {
        let p = |fx: f32, fy: f32| {
            Pos2::new(brect.min.x + bs * fx, brect.min.y + bs * fy)
        };
        painter.add(Shape::line(
            vec![p(0.22, 0.55), p(0.44, 0.74), p(0.80, 0.28)],
            Stroke::new(2.0 * s, Color32::WHITE.gamma_multiply(fade)),
        ));
    }
    painter.text(
        Pos2::new(brect.max.x + 10.0 * s, y),
        Align2::LEFT_CENTER,
        label,
        fid,
        Color32::from_gray(225).gamma_multiply(fade),
    );
    changed
}

/// `PillProgressSlider`: a thin rounded track filled from the left,
/// click/drag anywhere on it to set the value. `reset` is the value
/// a double-click restores (Lacrima's DoubleTapped reset); None
/// disables it.
#[allow(clippy::too_many_arguments)]
fn pill_slider(
    ui: &mut egui::Ui,
    track: Rect,
    salt: &str,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    fade: f32,
    reset: Option<f32>,
) -> bool {
    // Hit area covers the slider's 28px row around the 16px track.
    let hit = Rect::from_center_size(
        track.center(),
        Vec2::new(track.width(), track.height() * 1.75),
    );
    let resp = ui.interact(hit, ui.id().with(salt), Sense::click_and_drag());
    // Pointer-down reacts immediately — value + focus on press, not on
    // the drag threshold/release, so the slider feels instant.
    let pressed = resp.is_pointer_button_down_on();
    if pressed {
        resp.request_focus();
    }
    let mut changed = false;
    if resp.double_clicked() {
        if let Some(d) = reset {
            let nv = d.clamp(min, max);
            if nv != *value {
                *value = nv;
                changed = true;
            }
        }
    } else if pressed || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let f = ((p.x - track.min.x) / track.width()).clamp(0.0, 1.0);
            let nv = ((min + f * (max - min)) / step).round() * step;
            let nv = nv.clamp(min, max);
            if nv != *value {
                *value = nv;
                changed = true;
            }
        }
    }
    // Arrow keys nudge a focused slider: Up/Right = +step, Down/Left =
    // -step, Shift = 10x — the Avalonia Slider keyboard behavior. The
    // event filter claims the arrows for this widget (egui's own
    // Slider does the same) so focus navigation skips them; the
    // presses are also consumed so nothing else reacts.
    if resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
        let (up, down, shift) = ui.ctx().input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                i.modifiers.shift,
            )
        });
        let dir = if up {
            1.0
        } else if down {
            -1.0
        } else {
            0.0
        };
        if dir != 0.0 {
            let k = step * if shift { 10.0 } else { 1.0 };
            let nv = (*value + dir * k).clamp(min, max);
            if nv != *value {
                *value = nv;
                changed = true;
            }
            ui.ctx().request_repaint();
        }
        if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            resp.surrender_focus();
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let radius = CornerRadius::same((track.height() / 2.0) as u8);
    let painter = ui.painter();
    painter.rect_filled(
        track,
        radius,
        Color32::from_black_alpha(120).gamma_multiply(fade),
    );
    let frac = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    if frac > 0.0 {
        // Clip the pill-shaped fill to the filled span: the left cap stays
        // rounded, the right edge cuts straight at the value position, and
        // tiny values show a sliver instead of a degenerate rounded rect.
        let fill = Rect::from_min_max(
            track.min,
            Pos2::new(track.min.x + track.width() * frac, track.max.y),
        );
        painter.with_clip_rect(fill).rect_filled(
            track,
            radius,
            Color32::from_white_alpha(140).gamma_multiply(fade),
        );
    }
    painter.rect_stroke(
        track,
        radius,
        Stroke::new(
            if resp.has_focus() { 2.0 } else { 1.0 },
            Color32::from_gray(if resp.has_focus() { 235 } else { 160 })
                .gamma_multiply(fade),
        ),
        StrokeKind::Inside,
    );
    changed
}

/// Result of a confirmation dialog for this frame.
#[derive(Clone, Copy, PartialEq)]
enum DialogAction {
    None,
    Confirm,
    Cancel,
}
/// Keyboard media buttons — egui-winit drops VK_MEDIA_* (they never
/// reach `egui::Key`), so pressed-edges are polled via
/// GetAsyncKeyState: bit 0 of the return is "pressed since the
/// previous call", which is exactly the tap event we want.
enum MediaKey {
    PlayPause,
    Stop,
    Next,
    Prev,
}

#[cfg(windows)]
fn media_key_events(mut f: impl FnMut(MediaKey)) {
    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(v_key: i32) -> i16;
    }
    const MAP: [(i32, MediaKey); 4] = [
        (0xB0, MediaKey::Next),     // VK_MEDIA_NEXT_TRACK
        (0xB1, MediaKey::Prev),     // VK_MEDIA_PREV_TRACK
        (0xB2, MediaKey::Stop),     // VK_MEDIA_STOP
        (0xB3, MediaKey::PlayPause),// VK_MEDIA_PLAY_PAUSE
    ];
    unsafe {
        for (vk, k) in MAP {
            if GetAsyncKeyState(vk) & 1 != 0 {
                f(k);
            }
        }
    }
}

#[cfg(not(windows))]
fn media_key_events(_f: impl FnMut(MediaKey)) {}

/// The Switch-style confirmation dialog: a translucent veil over the
/// live content, a centered dialog, and two focusable buttons.
/// Tab/arrows move the focus (the focus glow lerps between buttons
/// as a visual flash), hover also moves it, Enter/click runs the
/// focused button; Esc closes (handled by the caller). Shared by the
/// exit-fullscreen and delete prompts — `salt` keeps the interaction
/// and animation ids distinct.
#[allow(clippy::too_many_arguments)]
fn confirm_overlay(
    ui: &mut egui::Ui,
    rect: Rect,
    title: &str,
    subtitle: &str,
    confirm_label: &str,
    cancel_label: &str,
    sel: &mut usize,
    s: f32,
    vis: f32,
    interactive: bool,
    salt: &str,
) -> DialogAction {
    let mut act = DialogAction::None;
    if interactive {
        ui.ctx().input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
            {
                *sel = 1 - *sel;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                act = if *sel == 0 {
                    DialogAction::Confirm
                } else {
                    DialogAction::Cancel
                };
            }
        });
    }

    let ctx = ui.ctx().clone();
    let painter = ui.painter().clone();
    // Translucent veil over the live content — alpha follows `vis`
    // so it fades in AND out.
    painter.rect_filled(
        rect,
        CornerRadius::ZERO,
        Color32::from_black_alpha((170.0 * vis) as u8),
    );

    // Slight scale-in — the dialog pops like a Switch prompt.
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
        subtitle,
        FontId::proportional(13.0 * s),
        Color32::from_gray(190).gamma_multiply(vis),
    );

    let bw = 140.0 * s;
    let bh = 38.0 * s;
    let by = dlg.min.y + 108.0 * s * pop;
    for (i, label) in [confirm_label, cancel_label].iter().enumerate() {
        let cx = dlg.center().x + (i as f32 - 0.5) * (bw + 20.0 * s) * pop;
        let b = Rect::from_center_size(Pos2::new(cx, by + bh * 0.5), Vec2::new(bw, bh));
        if interactive {
            let resp = ui.interact(b, ui.id().with((salt, "btn", i)), Sense::click());
            if resp.hovered() {
                *sel = i;
                ctx.set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                act = if i == 0 {
                    DialogAction::Confirm
                } else {
                    DialogAction::Cancel
                };
            }
        }
        // The focused button's glow lerps in — reads as the selection
        // flash moving between buttons.
        let hot = ctx.animate_bool_with_time(ui.id().with((salt, "sel", i)), *sel == i, 0.15);
        let br = CornerRadius::same((8.0 * s).round() as u8);
        painter.rect_filled(
            b,
            br,
            Color32::from_white_alpha(((40.0 + 215.0 * hot) * vis) as u8),
        );
        painter.rect_stroke(
            b,
            br,
            Stroke::new(
                (1.0 + hot) * s,
                Color32::from_white_alpha(((90.0 + 165.0 * hot) * vis) as u8),
            ),
            StrokeKind::Inside,
        );
        if hot > 0.01 {
            painter.rect_stroke(
                b.expand((4.0 * hot) * s),
                CornerRadius::same((11.0 * s).round() as u8),
                Stroke::new(
                    1.5 * s,
                    Color32::from_white_alpha((80.0 * hot * vis) as u8),
                ),
                StrokeKind::Inside,
            );
        }
        let g = (255.0 * (1.0 - hot)) as u8;
        painter.text(
            b.center(),
            Align2::CENTER_CENTER,
            *label,
            FontId::proportional(15.0 * s),
            Color32::from_rgb(g, g, g).gamma_multiply(vis),
        );
    }
    act
}

#[derive(Clone, Copy)]
enum TitleButton {
    Minimize,
    Fullscreen,
    FullscreenExit,
    Close,
}

fn title_button(ui: &mut egui::Ui, kind: TitleButton, scale: f32) -> egui::Response {
    // 35x30 hit area (MainTopBar buttons).
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(35.0 * scale, 30.0 * scale), Sense::click());
    let painter = ui.painter();
    let hovered = response.hovered();

    if hovered {
        let fill = match kind {
            TitleButton::Close => Color32::from_rgb(0xC4, 0x2B, 0x1C),
            _ => Color32::from_white_alpha(20),
        };
        painter.rect_filled(rect, CornerRadius::same((6.0 * scale).round() as u8), fill);
    }

    let color = if hovered {
        Color32::WHITE
    } else {
        Color32::from_rgb(0xDD, 0xDD, 0xDD)
    };
    let s = scale;
    let glyph = Stroke::new(2.0 * s, color);
    let c = rect.center();
    match kind {
        // 17x2.5 bar, 10px above the button's bottom edge.
        TitleButton::Minimize => {
            painter.line_segment(
                [c + egui::vec2(-8.5 * s, 5.0 * s), c + egui::vec2(8.5 * s, 5.0 * s)],
                Stroke::new(2.5 * s, color),
            );
        }
        // Big-picture mode icon: four corner brackets. Enter draws
        // them hugging the glyph's outer box with arms running along
        // the edges toward the middle (expand); exit draws the elbows
        // near the center with arms pointing back out (collapse).
        TitleButton::Fullscreen | TitleButton::FullscreenExit => {
            let exit = matches!(kind, TitleButton::FullscreenExit);
            let elbow = if exit { 2.5 * s } else { 7.0 * s };
            let dir = if exit { 1.0 } else { -1.0 };
            let arm = 4.5 * s;
            for sx in [-1.0f32, 1.0] {
                for sy in [-1.0f32, 1.0] {
                    let p = c + egui::vec2(sx * elbow, sy * elbow);
                    painter.line_segment([p, p + egui::vec2(dir * sx * arm, 0.0)], glyph);
                    painter.line_segment([p, p + egui::vec2(0.0, dir * sy * arm)], glyph);
                }
            }
        }
        TitleButton::Close => {
            painter.line_segment(
                [c + egui::vec2(-7.0 * s, -7.0 * s), c + egui::vec2(7.0 * s, 7.0 * s)],
                glyph,
            );
            painter.line_segment(
                [c + egui::vec2(7.0 * s, -7.0 * s), c + egui::vec2(-7.0 * s, 7.0 * s)],
                glyph,
            );
        }
    }
    response
}

/// Flat button in the settings-overlay style: solid rect, centered
/// label, hover/press feedback; Lacrima's themed Buttons use
/// `Padding="15,8"`. Returns (clicked, width) so callers can advance
/// a left-to-right row.
fn flat_button(
    ui: &mut egui::Ui,
    origin: Pos2,
    label: &str,
    s: f32,
    enabled: bool,
    bg: Color32,
    fg: Color32,
    t: f32,
) -> (bool, f32) {
    let painter = ui.painter();
    let font = FontId::proportional(14.0 * s);
    let tw = painter
        .layout_no_wrap(label.to_owned(), font.clone(), fg)
        .rect
        .width();
    let rect = Rect::from_min_size(origin, Vec2::new(tw + 30.0 * s, 30.0 * s));
    let resp = ui.interact(rect, ui.id().with(("tool-btn", label)), Sense::click());
    let fill = if !enabled {
        bg.gamma_multiply(0.35)
    } else if resp.is_pointer_button_down_on() {
        Color32::from_rgb(
            bg.r().saturating_sub(20),
            bg.g().saturating_sub(20),
            bg.b().saturating_sub(20),
        )
    } else if resp.hovered() {
        Color32::from_rgb(
            bg.r().saturating_add(22),
            bg.g().saturating_add(22),
            bg.b().saturating_add(22),
        )
    } else {
        bg
    };
    painter.rect_filled(rect, CornerRadius::same(4), fill.gamma_multiply(t));
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font,
        fg.gamma_multiply(t * if enabled { 1.0 } else { 0.45 }),
    );
    if resp.hovered() && enabled {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    (enabled && resp.clicked(), rect.width())
}

/// Libraries tab: Lacrima's "External tools" section — this build
/// only needs the yt-dlp block (no FFmpeg/libmpv/gamescope). Status
/// text, current/update versions, the Install/Update/Uninstall/
/// Check-for-Updates row and the busy progress bar all run through
/// [`ytdlp::YtDlp`], matching YtDlpManager's SettingsOverlay binding.
fn ui_tab_libraries(
    ui: &mut egui::Ui,
    panel: Rect,
    t: f32,
    ytdlp: &mut ytdlp::YtDlp,
    s: f32,
) {
    ytdlp.ensure_queried();
    let painter = ui.painter().clone();
    let x = panel.min.x + 30.0 * s;
    let right = panel.max.x - 30.0 * s;
    let mut y = panel.min.y + 95.0 * s;
    let whitesmoke = Color32::from_rgb(0xF5, 0xF5, 0xF5);
    let darkgray = Color32::from_gray(0xA9);

    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "External tools",
        FontId::proportional(30.0 * s),
        Color32::WHITE.gamma_multiply(t),
    );
    y += 46.0 * s;
    painter.line_segment(
        [Pos2::new(x, y), Pos2::new(right, y)],
        Stroke::new(1.0, Color32::from_gray(55).gamma_multiply(t)),
    );
    y += 25.0 * s;

    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "yt-dlp Status",
        FontId::proportional(20.0 * s),
        whitesmoke.gamma_multiply(t),
    );
    y += 30.0 * s;
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        &ytdlp.status,
        FontId::proportional(16.0 * s),
        darkgray.gamma_multiply(t),
    );
    y += 36.0 * s;

    // Version columns: CURRENT VERSION, then UPDATE AVAILABLE in gold
    // (Lacrima's two-StackPanel row with 40px spacing).
    let small = FontId::proportional(14.0 * s);
    let big = FontId::proportional(18.0 * s);
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        "CURRENT VERSION",
        small.clone(),
        darkgray.gamma_multiply(t),
    );
    let version = ytdlp.version.clone().unwrap_or_else(|| "Not Found".to_string());
    let vrect = painter.text(
        Pos2::new(x, y + 20.0 * s),
        Align2::LEFT_TOP,
        &version,
        big.clone(),
        whitesmoke.gamma_multiply(t),
    );
    let col_w = vrect.width().max(
        painter
            .layout_no_wrap("CURRENT VERSION".to_string(), small.clone(), Color32::WHITE)
            .rect
            .width(),
    );
    if ytdlp.update_available {
        let ux = x + col_w + 40.0 * s;
        painter.text(
            Pos2::new(ux, y),
            Align2::LEFT_TOP,
            "UPDATE AVAILABLE",
            small,
            Color32::GOLD.gamma_multiply(t),
        );
        painter.text(
            Pos2::new(ux, y + 20.0 * s),
            Align2::LEFT_TOP,
            ytdlp.update_version.as_deref().unwrap_or(""),
            big,
            Color32::GOLD.gamma_multiply(t),
        );
    }
    y += 56.0 * s;

    // Button row (Spacing 10): visibility mirrors the XAML bindings —
    // Install when missing, Update when an update exists, Uninstall
    // when installed, Check for Updates always; all disabled while busy.
    let gray = Color32::from_gray(0x3C);
    let busy = ytdlp.busy;
    let mut bx = x;
    if !ytdlp.installed {
        let (clicked, w) = flat_button(
            ui,
            Pos2::new(bx, y),
            "Install yt-dlp",
            s,
            !busy,
            gray,
            Color32::WHITE,
            t,
        );
        bx += w + 10.0 * s;
        if clicked {
            ytdlp.install_or_update();
        }
    }
    if ytdlp.update_available {
        let (clicked, w) = flat_button(
            ui,
            Pos2::new(bx, y),
            "Update",
            s,
            !busy,
            Color32::GOLD,
            Color32::BLACK,
            t,
        );
        bx += w + 10.0 * s;
        if clicked {
            ytdlp.install_or_update();
        }
    }
    if ytdlp.installed {
        let (clicked, w) = flat_button(
            ui,
            Pos2::new(bx, y),
            "Uninstall",
            s,
            !busy,
            Color32::from_rgb(0xF4, 0x43, 0x36),
            Color32::WHITE,
            t,
        );
        bx += w + 10.0 * s;
        if clicked {
            ytdlp.uninstall();
        }
    }
    let (clicked, _) = flat_button(
        ui,
        Pos2::new(bx, y),
        "Check for Updates",
        s,
        !busy,
        gray,
        Color32::WHITE,
        t,
    );
    if clicked {
        ytdlp.refresh();
    }
    y += 62.0 * s;

    // Lacrima's 2px busy bar (400px wide): real download progress when
    // the transfer reports a size, indeterminate otherwise.
    if busy {
        let bar = Rect::from_min_size(Pos2::new(x, y), Vec2::new(400.0 * s, 2.0 * s));
        painter.rect_filled(bar, 0, Color32::from_gray(60).gamma_multiply(t));
        if ytdlp.progress > 0.0 {
            let fill = Rect::from_min_size(
                bar.min,
                Vec2::new(bar.width() * ytdlp.progress.clamp(0.0, 1.0), bar.height()),
            );
            painter.rect_filled(fill, 0, Color32::from_gray(220).gamma_multiply(t));
        } else {
            let phase = (ui.ctx().input(|i| i.time) % 1.4) as f32 / 1.4;
            let w = bar.width() * 0.3;
            let sx = bar.min.x + (bar.width() - w) * phase;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(sx, bar.min.y), Vec2::new(w, bar.height())),
                0,
                Color32::from_gray(220).gamma_multiply(t),
            );
        }
        ui.ctx().request_repaint();
    }
}
