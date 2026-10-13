//! 10-band parametric EQ: RBJ-cookbook peaking biquads, one chain per
//! channel. Band gains are read from shared atomics each output frame
//! and only dirty bands get their coefficients rebuilt.

use std::sync::atomic::{AtomicU32, Ordering};

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
