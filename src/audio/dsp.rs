//! 10-band parametric EQ: RBJ-cookbook peaking biquads, one chain per
//! channel. Band gains are read from shared atomics each output frame
//! and only dirty bands get their coefficients rebuilt.

use rustfft::num_complex::Complex32;
use rustfft::Fft;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// ISO center frequencies for the 10 bands.
pub const EQ_BANDS: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
pub const EQ_MIN_DB: f32 = -12.0;
pub const EQ_MAX_DB: f32 = 12.0;
const BAND_Q: f32 = 1.2;

/// Transposed direct-form II biquad.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// RBJ peaking-EQ coefficients, `db` in EQ_MIN_DB..EQ_MAX_DB.
    fn peaking(sample_rate: f32, f0: f32, db: f32) -> Self {
        let f0 = f0.min(sample_rate * 0.45);
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / sample_rate;
        let alpha = w0.sin() / (2.0 * BAND_Q);
        let cos = w0.cos();
        let norm = 1.0 / (1.0 + alpha / a);
        Self {
            b0: (1.0 + alpha * a) * norm,
            b1: -2.0 * cos * norm,
            b2: (1.0 - alpha * a) * norm,
            a1: -2.0 * cos * norm,
            a2: (1.0 - alpha / a) * norm,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Stereo EQ chain. `set_gain`/`sync_gains` rebuild coefficients for a
/// band while keeping filter state, so live adjustments don't click.
pub struct Equalizer {
    chains: [[Biquad; 10]; 2],
    gains: [f32; 10],
    sample_rate: f32,
}

impl Equalizer {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            chains: [[Biquad::default(); 10]; 2],
            gains: [f32::NAN; 10], // force a rebuild on first sync
            sample_rate,
        }
    }

    /// Pull desired gains (dB) from shared atomics; rebuilds the coeffs
    /// of any band whose gain changed. Called once per output callback.
    pub fn sync_gains(&mut self, shared_gains: &[AtomicU32; 10]) {
        for band in 0..10 {
            let g = f32::from_bits(shared_gains[band].load(Ordering::Relaxed))
                .clamp(EQ_MIN_DB, EQ_MAX_DB);
            if g != self.gains[band] {
                self.gains[band] = g;
                let q = Biquad::peaking(self.sample_rate, EQ_BANDS[band], g);
                self.chains[0][band] = q;
                self.chains[1][band] = q;
            }
        }
    }

    pub fn gains(&self) -> [f32; 10] {
        self.gains.map(|g| if g.is_nan() { 0.0 } else { g })
    }

    #[inline]
    pub fn process(&mut self, l: &mut f32, r: &mut f32) {
        for band in 0..10 {
            *l = self.chains[0][band].process(*l);
            *r = self.chains[1][band].process(*r);
        }
    }
}

// -------------------------------------------------------------------
// Spectral-subtraction denoiser ("digital noise reduction" for AI-
// generated tracks). Neural codecs (Suno/Udio-style) leave a
// stationary broadband shimmer — strongest in the highs — that a
// per-bin noise-floor estimate + oversubtraction gain mask strips in
// real time. Runs on the decode thread between the resampler and the
// PCM ring, so FFT cost never touches the audio callback.
// -------------------------------------------------------------------

/// STFT geometry: 1024-sample frames, 50% overlap — a ~11ms hop at
/// 48kHz. Larger frames smear transients into a reverb-ish wash; this
/// keeps attack detail sharp. sqrt-Hann on analysis AND synthesis
/// gives a Hann product window, exactly COLA=1 at 50% overlap.
const DN_N: usize = 1024;
const DN_HOP: usize = DN_N / 2;
/// Oversubtraction factor: a bin passes the fraction of its magnitude
/// sitting above floor*OVER_SUB. AI shimmer hangs ~6-20dB under the
/// mix, so a mild over-subtract removes it without hollowing music.
const DN_OVER_SUB: f32 = 1.3;
/// Per-bin gain floor (~-9 dB). A deep floor makes the residual noise
/// pump in and out around musical events — that "hall/reverb" tail.
/// A shallow cut keeps the bed constant = natural, at the cost of
/// removing less hiss.
const DN_GAIN_MIN: f32 = 0.35;
/// Minimum-statistics floor: magnitude history of the last ~0.75s per
/// bin (64 hops), floor = the ~15th percentile. Music has micro-
/// dynamics — every bin dips between beats — so the quantile locks
/// onto the true noise level, while sustained loud bins are never
/// dragged down (the earlier leaky-average floor was).
const DN_HIST: usize = 64;
const DN_FLOOR_SMOOTH: f32 = 0.15;
/// Asymmetric temporal smoothing on the gain mask: rises fast to keep
/// transients intact, falls quickly enough that the noise gate shuts
/// before a blooming tail reads as reverb.
const DN_GAIN_UP: f32 = 0.6;
const DN_GAIN_DOWN: f32 = 0.35;

pub struct Denoiser {
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// De-interleaved input waiting to fill a hop.
    pend: [Vec<f32>; 2],
    /// N-sample sliding analysis window per channel.
    shift: [Vec<f32>; 2],
    /// OLA accumulator per channel; first HOP samples are finished
    /// output after each hop.
    ola: [Vec<f32>; 2],
    scratch: [Vec<Complex32>; 2],
    /// Per-bin magnitude history ring (bin-major: bin*DN_HIST + slot)
    /// feeding the minimum-statistics floor.
    hist: Vec<f32>,
    hist_pos: usize,
    /// Reusable scratch for the per-bin quantile select.
    qscratch: Vec<f32>,
    /// Per-bin noise floor + smoothed gain (N/2+1 real-FFT bins).
    floor: Vec<f32>,
    gain_sm: Vec<f32>,
    gain_out: Vec<f32>,
    /// rustfft's inverse is unscaled — the 1/N goes here.
    norm: f32,
}

impl Denoiser {
    pub fn new() -> Self {
        let mut planner = rustfft::FftPlanner::new();
        Self {
            fft: planner.plan_fft_forward(DN_N),
            ifft: planner.plan_fft_inverse(DN_N),
            window: (0..DN_N)
                .map(|i| {
                    (0.5 * (1.0
                        - (2.0 * std::f32::consts::PI * i as f32 / (DN_N - 1) as f32).cos()))
                    .sqrt()
                })
                .collect(),
            pend: [Vec::new(), Vec::new()],
            shift: [vec![0.0; DN_N], vec![0.0; DN_N]],
            ola: [vec![0.0; DN_N], vec![0.0; DN_N]],
            scratch: [vec![Complex32::ZERO; DN_N], vec![Complex32::ZERO; DN_N]],
            hist: vec![0.0; (DN_N / 2 + 1) * DN_HIST],
            hist_pos: 0,
            qscratch: vec![0.0; DN_HIST],
            floor: vec![0.0; DN_N / 2 + 1],
            gain_sm: vec![1.0; DN_N / 2 + 1],
            gain_out: vec![1.0; DN_N / 2 + 1],
            norm: 1.0 / DN_N as f32,
        }
    }

    /// Clear all signal state (seek/track change) so old content can't
    /// smear across the boundary.
    pub fn reset(&mut self) {
        self.pend = [Vec::new(), Vec::new()];
        for v in self.shift.iter_mut().chain(self.ola.iter_mut()) {
            v.fill(0.0);
        }
        self.hist.fill(0.0);
        self.hist_pos = 0;
        self.floor.fill(0.0);
        self.gain_sm.fill(1.0);
        self.gain_out.fill(1.0);
    }

    /// Feed interleaved stereo at the output rate; appends processed
    /// interleaved frames to `out`. Disabled = straight passthrough
    /// (plus a reset so re-engaging starts on a clean estimate).
    pub fn push(&mut self, stereo: &[f32], enabled: bool, out: &mut Vec<f32>) {
        if !enabled {
            self.reset();
            out.extend_from_slice(stereo);
            return;
        }
        for pair in stereo.chunks_exact(2) {
            self.pend[0].push(pair[0]);
            self.pend[1].push(pair[1]);
        }
        while self.pend[0].len() >= DN_HOP {
            self.hop(out);
        }
    }

    /// One STFT hop: slide both analysis windows, build a shared gain
    /// mask from the combined magnitude (keeps the stereo image glued),
    /// apply it to each channel, IFFT and overlap-add HOP new samples.
    fn hop(&mut self, out: &mut Vec<f32>) {
        for c in 0..2 {
            self.shift[c].copy_within(DN_HOP.., 0);
            self.shift[c][DN_N - DN_HOP..].copy_from_slice(&self.pend[c][..DN_HOP]);
            self.pend[c].drain(..DN_HOP);
            for i in 0..DN_N {
                self.scratch[c][i] =
                    Complex32::new(self.shift[c][i] * self.window[i], 0.0);
            }
            self.fft.process(&mut self.scratch[c]);
        }

        // Noise floor + gain mask on the combined magnitude spectrum.
        // Each bin's floor is the low quantile of its recent magnitude
        // window — the level it dips to between hits, which for
        // stationary shimmer IS the noise.
        let nb = DN_N / 2 + 1;
        for i in 0..nb {
            let m = self.scratch[0][i].norm() + self.scratch[1][i].norm();
            self.hist[i * DN_HIST + self.hist_pos] = m;
            self.qscratch
                .copy_from_slice(&self.hist[i * DN_HIST..(i + 1) * DN_HIST]);
            let k = DN_HIST * 15 / 100;
            let q = *self
                .qscratch
                .select_nth_unstable_by(k, |a, b| a.partial_cmp(b).unwrap())
                .1;
            self.floor[i] += (q - self.floor[i]) * DN_FLOOR_SMOOTH;
            let clean = (m - DN_OVER_SUB * self.floor[i]).max(0.0);
            let g = (clean / m.max(1e-9)).clamp(DN_GAIN_MIN, 1.0);
            let k2 = if g > self.gain_sm[i] {
                DN_GAIN_UP
            } else {
                DN_GAIN_DOWN
            };
            self.gain_sm[i] += (g - self.gain_sm[i]) * k2;
        }
        self.hist_pos = (self.hist_pos + 1) % DN_HIST;
        // 5-bin boxcar across frequency — isolated-bin pumping is what
        // makes spectral subtraction sound watery.
        for i in 0..=DN_N / 2 {
            let lo = i.saturating_sub(2);
            let hi = (i + 2).min(DN_N / 2);
            self.gain_out[i] =
                self.gain_sm[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
        }

        for c in 0..2 {
            let spec = &mut self.scratch[c];
            for i in 0..DN_N {
                spec[i] *= self.gain_out[i.min(DN_N - i)];
            }
            self.ifft.process(spec);
            for i in 0..DN_N {
                self.ola[c][i] += spec[i].re * self.window[i] * self.norm;
            }
        }
        for i in 0..DN_HOP {
            out.push(self.ola[0][i]);
            out.push(self.ola[1][i]);
        }
        for c in 0..2 {
            self.ola[c].copy_within(DN_HOP.., 0);
            self.ola[c][DN_N - DN_HOP..].fill(0.0);
        }
    }
}
