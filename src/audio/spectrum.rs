//! Spectrum analyzer: post-EQ stereo PCM is tapped into a ring; the UI
//! drains it, windows it (Hann), FFTs, and folds bins into
//! `SPECTRUM_BARS` log-spaced bands with attack/decay smoothing.

use rustfft::num_complex::Complex32;
use rustfft::Fft;
use std::sync::Arc;

pub const SPECTRUM_BARS: usize = 48;
/// Raw FFT bins exposed for the ShaderToy spectrum texture —
/// `FFMpegSpectrumAnalyzer` pushes 256 raw log-magnitudes into
/// `AudioPlayer.Spectrum`, and `GlShaderToyControl` stretches them
/// to 512. Feeding the folded 48-band display bars instead leaves
/// ~10-bin-wide steps that read as disconnected wave segments.
pub const SHADER_BINS: usize = 512;
const FFT_SIZE: usize = 2048;
/// Analysis range; upper edge also clamps to 90% of Nyquist.
/// Lacrima's analyzer reads 256 linear bins of a 1024-pt FFT at
/// 44.1 kHz — a 0..~11 kHz span (256 * 43.066 Hz). Bands placed above
/// ~11k only measure codec lowpass (AAC/MP3 cut ~15-16k) and render
/// as a dead tail, so the display range matches Lacrima's coverage.
const MIN_FREQ: f32 = 30.0;
const MAX_FREQ: f32 = 11_025.0;
/// Per-frame smoothing: rise fast, fall slowly.
const ATTACK: f32 = 0.55;
const DECAY: f32 = 0.12;
/// dB window mapped to 0..1 bar heights.
const DB_FLOOR: f32 = -60.0;

pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Circular mono-sample buffer, last FFT_SIZE samples.
    ring: Vec<f32>,
    write: usize,
    filled: usize,
    scratch: Vec<Complex32>,
    /// Log band edges in FFT bins, precomputed from the output rate.
    bins: Vec<(usize, usize)>,
    bars: Vec<f32>,
    /// Per-bin log magnitudes for the ShaderToy `iChannel0` texture.
    shader: Vec<f32>,
}

impl Spectrum {
    pub fn new(sample_rate: f32) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| {
                0.5 * (1.0
                    - (2.0 * std::f32::consts::PI * i as f32 / (FFT_SIZE - 1) as f32).cos())
            })
            .collect();

        // 48 log-spaced bands over [MIN_FREQ, min(MAX_FREQ, nyquist*0.9)].
        let hi = MAX_FREQ.min(sample_rate * 0.45);
        let (lo_log, hi_log) = (MIN_FREQ.log10(), hi.log10());
        let bin_hz = sample_rate / FFT_SIZE as f32;
        let mut bins = Vec::with_capacity(SPECTRUM_BARS);
        let mut prev_lo = (MIN_FREQ / bin_hz) as usize;
        for i in 1..=SPECTRUM_BARS {
            let f = 10f32.powf(lo_log + (hi_log - lo_log) * i as f32 / SPECTRUM_BARS as f32);
            let bin = ((f / bin_hz) as usize).max(prev_lo + 1).min(FFT_SIZE / 2);
            bins.push((prev_lo, bin));
            prev_lo = bin;
        }

        Self {
            fft,
            window,
            ring: vec![0.0; FFT_SIZE],
            write: 0,
            filled: 0,
            scratch: vec![Complex32::ZERO; FFT_SIZE],
            bins,
            bars: vec![0.0; SPECTRUM_BARS],
            shader: vec![0.0; SHADER_BINS],
        }
    }

    /// Feed interleaved stereo samples (post-EQ); averaged to mono.
    pub fn feed(&mut self, stereo: &[f32]) {
        for pair in stereo.chunks_exact(2) {
            self.ring[self.write] = (pair[0] + pair[1]) * 0.5;
            self.write = (self.write + 1) % FFT_SIZE;
            self.filled = (self.filled + 1).min(FFT_SIZE);
        }
    }

    /// Smoothed bar magnitudes in 0..1. Call once per UI frame; feeds
    /// nothing itself — pump `feed` from the spectrum ring first.
    pub fn bars(&mut self) -> &[f32] {
        if self.filled < FFT_SIZE / 4 {
            return &self.bars;
        }
        // Copy the ring chronologically and apply the window.
        for i in 0..FFT_SIZE {
            let idx = (self.write + i) % FFT_SIZE;
            self.scratch[i] = Complex32::new(self.ring[idx] * self.window[i], 0.0);
        }
        self.fft.process(&mut self.scratch);

        // Log10(mag+1)*20 like `AnalyzeSpectrum` — bins 0..512 span
        // 0..~11kHz at 44.1kHz (FFT_SIZE/2048), matching Lacrima's
        // 256 bins @ 1024-sample FFT over the same range.
        for (i, v) in self.shader.iter_mut().enumerate() {
            *v = 20.0 * (self.scratch[i].norm() + 1.0).log10();
        }

        for (i, &(lo, hi)) in self.bins.iter().enumerate() {
            let peak = self.scratch[lo..hi]
                .iter()
                .map(|c| c.norm())
                .fold(0.0f32, f32::max)
                / (FFT_SIZE as f32 * 0.5);
            let db = 20.0 * peak.max(1e-7).log10();
            let v = ((db - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0);
            let bar = &mut self.bars[i];
            *bar += if v > *bar {
                (v - *bar) * ATTACK
            } else {
                (v - *bar) * DECAY
            };
        }
        &self.bars
    }

    /// Raw log-magnitude FFT bins for the ShaderToy spectrum texture —
    /// populated by `bars()` (same FFT pass), so call it after feeding.
    pub fn shader_bins(&self) -> &[f32] {
        &self.shader
    }
}
