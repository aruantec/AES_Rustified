//! Audio layer: symphonia decode -> rubato resample -> SPSC ring buffer
//! -> cpal output. The output path runs the EQ chain (post-tap feeds the
//! spectrum analyzer), so visuals reflect what actually plays.
//!
//! Layout: `player` owns the cpal stream and the decode thread;
//! `dsp` is the 10-band biquad EQ; `spectrum` turns PCM into bars.

#![allow(unused)]

mod dsp;
mod player;
mod spectrum;
pub(crate) mod stream_cache;
pub mod waveform;

pub use dsp::{Equalizer, EQ_BANDS, EQ_MAX_DB, EQ_MIN_DB};
pub use player::{AudioPlayer, PlayerState};
pub use spectrum::{Spectrum, SPECTRUM_BARS};
