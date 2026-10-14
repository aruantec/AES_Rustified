//! Persistent settings, saved to
//! `%LOCALAPPDATA%\AES_Rustified\Settings\Settings.json` on Windows
//! (the equivalent per-OS data dir elsewhere).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const APP_DIR: &str = "AES_Rustified";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Settings {
    pub window: Window,
    pub clock: Clock,
    pub player: Player,
    pub effects: Effects,
    pub library: Library,
    pub spectrum: Spectrum,
    /// Cover-view presentation: Lacrima's CoverLayoutMode +
    /// CarouselSpacing/Scale/VerticalOffset/SideTranslation/
    /// StackSpacing/UseFullCoverSize and CardGrid scale/spacing.
    pub carousel: Carousel,
    /// 10-band equalizer gains, persisted so the DSP state restores.
    pub equalizer: Equalizer,
    /// Audio-tab "ReplayGain / Normalization" and "Playback Options"
    /// sections (Lacrima's SettingsViewModel audio settings).
    pub audio: Audio,
    /// Emulation-tab "Miscellaneous" + "Gameplay Recording" sections
    /// (Lacrima's EmulationUseFirstItemCover / EmulationGameplayAutoplay /
    /// EmulationUseBackCoverLetterboxFill / GameplayRecordingOutputDirectory).
    pub emulation: Emulation,
    pub ui: Ui,
    /// Whether `Ui.ScaleFactor` was actually present in the loaded JSON —
    /// Lacrima's `HasPersistedScaleFactor`. When false, the app picks the
    /// first-run default (display DPI, clamped 1..=2) instead of 1.0.
    #[serde(skip)]
    pub scale_factor_persisted: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Window {
    pub width: f32,
    pub height: f32,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            width: 960.0,
            height: 600.0,
        }
    }
}

/// Clock rect stored as fractions of the content area (the region
/// below the title bar), so it restores proportionally at any window
/// size. Values outside 0..=1.5 are treated as legacy absolute-unit
/// saves and fall back to the default layout.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Clock {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub visible: bool,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            left: 36.0,
            top: 28.0,
            width: 170.0,
            height: 110.0,
            visible: true,
        }
    }
}

/// UI-tab "Effects" options (the Lacrima rain layer).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Effects {
    pub show_lacrima: bool,
    /// Tint the rain drops with the cover palette (like the
    /// cover-colored shadertoys) instead of white.
    pub lacrima_cover_color: bool,
    pub lacrima_count: u32,
    pub show_edge_border: bool,
    pub animate_title: bool,
    /// UI-tab "ShaderToy" section (Lacrima's ShowShaderToy +
    /// SelectedShadertoy — the shader's file-stem name).
    pub show_shadertoy: bool,
    pub shadertoy: String,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            show_lacrima: true,
            lacrima_cover_color: false,
            lacrima_count: 100,
            show_edge_border: true,
            animate_title: true,
            show_shadertoy: true,
            // EnsureDefaultSelectedShaderToy prefers this name.
            shadertoy: "AnimatedBackground".to_string(),
        }
    }
}

/// Player turntable widget rect — same fractional semantics as
/// `Clock`; negative `left` means "not placed yet" so the widget
/// computes the Lacrima default (disc centered on the ShaderToy
/// origin).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Player {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub visible: bool,
}

impl Default for Player {
    fn default() -> Self {
        Self {
            left: -1.0,
            top: -1.0,
            width: 0.0,
            height: 0.0,
            visible: true,
        }
    }
}

/// Media library roots: folders scanned for album folders (a directory
/// that directly contains audio files becomes an album tile).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Library {
    pub music_roots: Vec<PathBuf>,
    /// Album dir paths in the user's drag order; applied on top of the
    /// scan order, with unknown (new) albums appended at the end.
    pub album_order: Vec<String>,
    /// URL media items per album dir (Lacrima persists url children
    /// with the album). Key = album dir string, value = added urls.
    pub url_items: std::collections::BTreeMap<String, Vec<String>>,
    /// Album dirs removed via "Delete Album" — filtered out of every
    /// scan so they don't resurrect on rescan.
    pub excluded_albums: Vec<String>,
    /// File tracks removed from an album via the card Delete or Clear
    /// Album — kept out of `Album.tracks` on every scan merge so
    /// deletions persist across reopen/rescan (Lacrima serializes the
    /// reduced Children list; our scan re-reads the folder, so the
    /// exclusion is what carries the deletion forward). Key = album
    /// dir string, value = removed track paths.
    pub excluded_tracks: std::collections::BTreeMap<String, Vec<String>>,
    /// Virtual albums — created via "Create Album" or imported from
    /// .apl files — no backing folder, re-created on top of every
    /// scan result. Both produce the identical record: `dir` is a
    /// synthetic "apl://..." key also used in `url_items`.
    #[serde(alias = "ImportedAlbums")]
    pub virtual_albums: Vec<VirtualAlbum>,
    /// "Rename Album" display-title overrides, keyed by album dir —
    /// folder names on disk are never touched, and virtual albums'
    /// `apl://` keys stay stable while their labels change.
    pub album_titles: std::collections::BTreeMap<String, String>,
    /// Last session's album list in display order (dirs incl. virtual
    /// keys) — written at each scan's end, read at startup so the row
    /// renders instantly in final order while the rescan streams the
    /// real albums into their slots.
    pub cached_albums: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct VirtualAlbum {
    pub dir: String,
    pub title: String,
}

impl Default for VirtualAlbum {
    fn default() -> Self {
        Self {
            dir: String::new(),
            title: String::new(),
        }
    }
}

/// Audio-tab "Spectrum" options for the music-view analyzer bars
/// (Lacrima's SpectrumHeight/BarWidth/BarSpacing/ShowMusicSpectrum/
/// UseMusicSpectrumCoverColors/SpectrumColor0..4 settings).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Spectrum {
    /// Bar stack height in virtual px (slider range 50..1000).
    pub height: f32,
    /// Logical bar width, rendered at 0.8x (slider 10..100).
    pub bar_width: f32,
    /// Gap between bars (slider 0..100).
    pub bar_spacing: f32,
    /// ShowMusicSpectrum — whether the bars draw at all.
    pub show: bool,
    /// UseMusicSpectrumCoverColors — derive the bar gradient from the
    /// playing album's cover instead of the configured stops.
    pub cover_colors: bool,
    /// SpectrumColor0..4: the manual 5-stop gradient (RGB).
    pub gradient: [[u8; 3]; 5],
}

impl Default for Spectrum {
    fn default() -> Self {
        Self {
            height: 170.0,
            bar_width: 15.0,
            bar_spacing: 5.0,
            show: true,
            cover_colors: true,
            // _presetSpectrumColors: the default rainbow.
            gradient: [
                [0x00, 0xCC, 0xFF],
                [0x33, 0x33, 0xFF],
                [0xCC, 0x00, 0xCC],
                [0xFF, 0x00, 0x4D],
                [0xFF, 0xB3, 0x00],
            ],
        }
    }
}

/// Cover-view presentation options for the loaded album's track items.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Carousel {
    /// CoverLayoutMode: 0 = Carousel, 1 = VerticalGrid, 2 = HorizontalGrid.
    pub layout_mode: u8,
    /// CarouselSpacing — spacing multiplier in the projection.
    pub spacing: f32,
    /// CarouselScale — item size multiplier.
    pub scale: f32,
    /// CarouselVerticalOffset — px the cover cluster sits above center.
    pub vertical_offset: f32,
    /// CarouselSideTranslation — how far side items fan outward.
    pub side_translation: f32,
    /// CarouselStackSpacing — extra per-index stack offset.
    pub stack_spacing: f32,
    /// CarouselUseFullCoverSize — render covers at natural aspect
    /// (clamped to 1.35) instead of a fixed square frame.
    pub use_full_cover_size: bool,
    /// CardGrid base card width in px (Lacrima's BaseCardWidth*CardScale
    /// = 200*1.88).
    pub card_size: f32,
    /// CardGridSpacing — px between cards.
    pub card_spacing: f32,
    /// CardGridOpacity — opacity multiplier on grid cards.
    pub card_opacity: f32,
    /// CardGridBackgroundColor — the content row's background (RGB).
    pub grid_bg: [u8; 3],
}

impl Default for Carousel {
    fn default() -> Self {
        Self {
            layout_mode: 0,
            spacing: 0.93,
            scale: 1.88,
            vertical_offset: -95.0,
            side_translation: 73.0,
            stack_spacing: 39.0,
            use_full_cover_size: true,
            card_size: 376.0,
            card_spacing: 16.0,
            card_opacity: 1.0,
            grid_bg: [0x10, 0x10, 0x10],
        }
    }
}

/// Audio-tab "ReplayGain / Normalization" + "Playback Options"
/// sections. Field names serialize to Lacrima's SettingsViewModel
/// property names so a shared Settings.json reads identically.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Audio {
    /// SmoothVolumeChange — ramp gain changes (~50ms) instead of
    /// snapping. (Lacrima stores it but its mpv backend snaps; ours
    /// actually ramps in the output callback.)
    pub smooth_volume_change: bool,
    /// LogarithmicVolumeControl — quadratic volume curve, more travel
    /// at low volumes (Lacrima squares the 0..100 level).
    pub logarithmic_volume_control: bool,
    /// LoudnessCompensatedVolume — ISO 226-inspired low-volume boost;
    /// Lacrima applies a 2/3-power mapping on top of the level.
    pub loudness_compensated_volume: bool,
    /// ReplayGainEnabled — master switch for all gain normalization.
    pub replay_gain_enabled: bool,
    /// ReplayGainAnalyzeOnTheFly — measure the track's loudness while
    /// decoding when no ReplayGain tag exists.
    pub replay_gain_analyze_on_the_fly: bool,
    /// ReplayGainUseTags — read REPLAYGAIN_* tags first.
    pub replay_gain_use_tags: bool,
    /// ReplayGainPreampDb — dB added to analyzed gains.
    pub replay_gain_preamp_db: f32,
    /// ReplayGainTagsPreampDb — dB added to tag gains.
    pub replay_gain_tags_preamp_db: f32,
    /// ReplayGainTagSource — 0 = Track, 1 = Album (the preferred tag
    /// tried first; the other is the fallback either way).
    pub replay_gain_tag_source: u8,
    /// RestartOnResumeEnabled — resuming a track that was paused
    /// within the first N seconds rewinds to the start.
    pub restart_on_resume_enabled: bool,
    /// RestartOnResumeThresholdSeconds (slider 0..20).
    pub restart_on_resume_threshold_seconds: f32,
    /// PauseWhenVolumeIsZero — setting the volume to 0 pauses; raising
    /// it again resumes.
    pub pause_when_volume_is_zero: bool,
    /// SilenceAdvanceDelayMs — wait after playback enters the track's
    /// trailing-silence region (measured off the generated waveform)
    /// before ending the track early.
    pub silence_advance_delay_ms: u32,
    /// AiNoiseReduction (Rustified-only — no Lacrima equivalent):
    /// spectral-subtraction denoiser for the broadband shimmer neural
    /// codecs leave in AI-generated tracks.
    pub ai_noise_reduction: bool,
    /// VolumeKnobColor (Rustified-only): accent color of the volume
    /// knob's level arc in the transport band.
    pub volume_knob_color: [u8; 3],
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            smooth_volume_change: true,
            logarithmic_volume_control: false,
            loudness_compensated_volume: true,
            replay_gain_enabled: false,
            replay_gain_analyze_on_the_fly: true,
            replay_gain_use_tags: true,
            replay_gain_preamp_db: 0.0,
            replay_gain_tags_preamp_db: 0.0,
            // Lacrima defaults the tag source ComboBox to "Album".
            replay_gain_tag_source: 1,
            restart_on_resume_enabled: false,
            restart_on_resume_threshold_seconds: 10.0,
            pause_when_volume_is_zero: false,
            silence_advance_delay_ms: 500,
            ai_noise_reduction: false,
            volume_knob_color: [0x3F, 0xD8, 0xE8],
        }
    }
}

/// Emulation tab settings (Lacrima's SettingsOverlay Emulation tab).
/// The per-console handler configuration is view-model state in the
/// prototype — only the plain options persist here.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Emulation {
    /// EmulationUseFirstItemCover — album tiles show the first ROM
    /// item's cover instead of the section art.
    pub use_first_item_cover: bool,
    /// EmulationGameplayAutoplay — start the gameplay preview the
    /// moment a cover is highlighted instead of after the dwell delay.
    pub gameplay_autoplay: bool,
    /// EmulationUseBackCoverLetterboxFill — fill the capture's
    /// letterbox bars with the game's back cover art.
    pub back_cover_letterbox_fill: bool,
    /// GameplayRecordingOutputDirectory — where recorded videos land.
    pub recording_dir: String,
}

impl Default for Emulation {
    fn default() -> Self {
        Self {
            use_first_item_cover: false,
            gameplay_autoplay: false,
            back_cover_letterbox_fill: false,
            // Lacrima: MyVideos/AES_Lacrima, else DataRoot/Recordings.
            recording_dir: dirs::video_dir()
                .map(|d| d.join("AES_Rustified").to_string_lossy().into_owned())
                .or_else(|| {
                    data_root().map(|d| d.join("Recordings").to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
        }
    }
}

/// Global UI scale (Lacrima's ScaleFactor — the "Scaling" slider in the
/// 10-band equalizer gains in dB (ISO centers 31Hz..16kHz, ±12dB) —
/// the mini-player style EQ drawer between transport and waveform.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Equalizer {
    pub bands: [f32; 10],
}

impl Default for Equalizer {
    fn default() -> Self {
        Self { bands: [0.0; 10] }
    }
}

/// UI tab, 1.0..=3.0). Zooms all chrome uniformly.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Ui {
    pub scale_factor: f32,
}

impl Default for Ui {
    fn default() -> Self {
        Self { scale_factor: 1.0 }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window: Window::default(),
            clock: Clock::default(),
            player: Player::default(),
            effects: Effects::default(),
            library: Library::default(),
            spectrum: Spectrum::default(),
            carousel: Carousel::default(),
            equalizer: Equalizer::default(),
            audio: Audio::default(),
            emulation: Emulation::default(),
            ui: Ui::default(),
            scale_factor_persisted: false,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| {
        d.join(APP_DIR)
            .join("Settings")
            .join("Settings.json")
    })
}

/// `%LOCALAPPDATA%\AES_Rustified` — the app's data root (Settings/,
/// Tools/, caches, Shaders/), opened by Settings' "Open AES
/// Directory" button.
pub fn data_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(APP_DIR))
}

pub fn load() -> Settings {
    let Some(path) = path() else { return Settings::default() };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    let mut settings = match serde_json::from_str::<Settings>(&text) {
        Ok(s) => s,
        Err(_) => {
            // Never let one bad/missing field wipe the whole config:
            // keep the unreadable file for recovery and fall back to
            // parsing each section independently (a broken section
            // alone returns its defaults).
            let _ = std::fs::copy(&path, path.with_extension("corrupt.json"));
            let mut s = Settings::default();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                section(&v, "Window", &mut s.window);
                section(&v, "Clock", &mut s.clock);
                section(&v, "Player", &mut s.player);
                section(&v, "Effects", &mut s.effects);
                section(&v, "Library", &mut s.library);
                section(&v, "Spectrum", &mut s.spectrum);
                section(&v, "Carousel", &mut s.carousel);
                section(&v, "Equalizer", &mut s.equalizer);
                section(&v, "Audio", &mut s.audio);
                section(&v, "Emulation", &mut s.emulation);
                section(&v, "Ui", &mut s.ui);
            }
            s
        }
    };
    settings.scale_factor_persisted = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("Ui")?.get("ScaleFactor").cloned())
        .is_some();
    settings
}

fn section<T: serde::de::DeserializeOwned>(v: &serde_json::Value, name: &str, out: &mut T) {
    if let Some(s) = v.get(name) {
        if let Ok(parsed) = serde_json::from_value::<T>(s.clone()) {
            *out = parsed;
        }
    }
}

pub fn save(settings: &Settings) {
    if let Some(p) = path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(settings) {
            // Write to a temp file then atomically swap — a crash or
            // force-close mid-write can never leave a truncated file.
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                if p.exists() {
                    let _ = std::fs::copy(&p, p.with_extension("bak"));
                }
                if std::fs::rename(&tmp, &p).is_err() {
                    let _ = std::fs::copy(&tmp, &p);
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
    }
}
