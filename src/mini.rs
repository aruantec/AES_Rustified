//! The mini shell — Lacrima's MinView skin over the same data: the
//! loaded album's items, the live player, the waveform seek bar, the
//! spectrum and the cover decode pool — nothing here owns playback
//! state. Content is intentionally blank for now: the window is just
//! the custom frame's top bar while the shell is reshaped.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, Rect, Vec2};

use crate::{album_row, audio, carousel, settings, spectrum_bar, transport, waveform_bar};

/// The mini window's floor — below this the rows stop fitting.
pub(crate) const MIN_SIZE: Vec2 = Vec2::new(340.0, 420.0);
/// First-run size when no mini geometry was persisted yet.
pub(crate) const DEFAULT_SIZE: Vec2 = Vec2::new(420.0, 560.0);

/// Playback intents the shell can't serve itself — the app's shared
/// action tail resolves them exactly like the Music view's outputs.
#[derive(Default)]
pub(crate) struct MiniOut {
    /// A track the user picked from the list, or a step-path result.
    pub play: Option<PathBuf>,
    /// A waveform scrub release, in seconds.
    pub seek: Option<f64>,
}

/// Paint + interact for one frame of the mini shell. `content` is the
/// area below the title bar; geometry follows the small window. All
/// content below the top bar is blank until the new layout lands.
#[allow(clippy::too_many_arguments)]
pub(crate) fn view(
    ui: &mut egui::Ui,
    content: Rect,
    _interactive: bool,
    _carousel: &carousel::Carousel,
    _row: &mut album_row::AlbumRow,
    _player: Option<&audio::AudioPlayer>,
    _playing_track: Option<&Path>,
    _stream_loading: bool,
    _waveform: &[f32],
    _waveform_loading: bool,
    _bar: &mut waveform_bar::WaveformBar,
    _transport: &mut transport::Transport,
    _spectrum: &spectrum_bar::SpectrumBar,
    _spec_cfg: &settings::Spectrum,
    grid_bg: Color32,
    _cover_colors: Option<&[Color32]>,
    _accent: Color32,
    _s: f32,
) -> MiniOut {
    ui.painter().rect_filled(content, 0, grid_bg);
    MiniOut::default()
}
