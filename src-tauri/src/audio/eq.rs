//! Ten-band parametric equalizer built from RBJ cookbook biquads.
//!
//! Runs inside the audio callback, so everything here is allocation-free and
//! branch-light: coefficients are recomputed only when a gain actually changes,
//! and a chain whose bands are all flat is skipped entirely.

/// Band centre frequencies, matching the UI sliders.
pub const FREQS: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
pub const BANDS: usize = FREQS.len();

/// Q for the peaking bands; the shelves use S = 1 instead.
const PEAK_Q: f32 = 1.1;

#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    LowShelf,
    Peaking,
    HighShelf,
}

impl Biquad {
    /// Transposed direct form II — one multiply-add chain, minimal state.
    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    #[inline]
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn design(kind: Kind, fs: f32, f0: f32, gain_db: f32) -> Self {
        // Above Nyquist a band has nothing to act on — leave it flat.
        if fs <= 0.0 || f0 >= fs * 0.5 {
            return Self::identity();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();

        let (b0, b1, b2, a0, a1, a2) = match kind {
            Kind::Peaking => {
                let alpha = sin_w0 / (2.0 * PEAK_Q);
                (
                    1.0 + alpha * a,
                    -2.0 * cos_w0,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cos_w0,
                    1.0 - alpha / a,
                )
            }
            Kind::LowShelf => {
                // S = 1
                let alpha = sin_w0 / 2.0 * std::f32::consts::SQRT_2;
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                    (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
            Kind::HighShelf => {
                let alpha = sin_w0 / 2.0 * std::f32::consts::SQRT_2;
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                    (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }
}

/// One equalizer chain per channel, plus the shared bypass decision.
pub struct Equalizer {
    bands: Vec<[Biquad; BANDS]>, // per channel
    gains: [f32; BANDS],
    preamp_lin: f32,
    enabled: bool,
    bypass: bool, // enabled but every band flat → skip the filter work
    sample_rate: f32,
}

impl Equalizer {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let mut eq = Self {
            bands: vec![[Biquad::identity(); BANDS]; channels.max(1)],
            gains: [0.0; BANDS],
            preamp_lin: 1.0,
            enabled: true,
            bypass: true,
            sample_rate: sample_rate as f32,
        };
        eq.redesign();
        eq
    }

    pub fn set_params(&mut self, gains: &[f32; BANDS], preamp_db: f32, enabled: bool) {
        self.gains = *gains;
        self.preamp_lin = 10f32.powf(preamp_db / 20.0);
        self.enabled = enabled;
        self.redesign();
    }

    fn redesign(&mut self) {
        let flat = self.gains.iter().all(|g| g.abs() < 0.05);
        self.bypass = !self.enabled || (flat && (self.preamp_lin - 1.0).abs() < 1e-4);
        if !self.enabled {
            return;
        }
        for chain in self.bands.iter_mut() {
            for (i, band) in chain.iter_mut().enumerate() {
                let kind = match i {
                    0 => Kind::LowShelf,
                    x if x == BANDS - 1 => Kind::HighShelf,
                    _ => Kind::Peaking,
                };
                let state = (band.z1, band.z2);
                *band = Biquad::design(kind, self.sample_rate, FREQS[i], self.gains[i]);
                // keep filter memory so gain changes don't click
                band.z1 = state.0;
                band.z2 = state.1;
            }
        }
    }

    pub fn reset(&mut self) {
        for chain in self.bands.iter_mut() {
            for band in chain.iter_mut() {
                band.reset();
            }
        }
    }

    /// Process interleaved frames in place.
    #[inline]
    pub fn process_interleaved(&mut self, data: &mut [f32], channels: usize) {
        if self.bypass || channels == 0 {
            return;
        }
        let preamp = self.preamp_lin;
        if !self.enabled {
            return;
        }
        for frame in data.chunks_mut(channels) {
            for (ch, sample) in frame.iter_mut().enumerate() {
                let chain = match self.bands.get_mut(ch) {
                    Some(c) => c,
                    None => break,
                };
                let mut v = *sample * preamp;
                for band in chain.iter_mut() {
                    v = band.process(v);
                }
                *sample = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a sine at `freq` through the chain and return its RMS gain in dB.
    fn gain_at(eq: &mut Equalizer, sample_rate: u32, freq: f32) -> f32 {
        let n = sample_rate as usize; // one second, long enough to settle
        let mut buf: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin())
            .collect();
        eq.reset();
        eq.process_interleaved(&mut buf, 1);
        // skip the first 10% so the filter transient is not measured
        let tail = &buf[n / 10..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        let ref_rms = (0.5f32).sqrt(); // RMS of a unit sine
        20.0 * (rms / ref_rms).log10()
    }

    #[test]
    fn flat_settings_pass_audio_through_untouched() {
        let mut eq = Equalizer::new(48_000, 2);
        eq.set_params(&[0.0; BANDS], 0.0, true);
        let original: Vec<f32> = (0..512).map(|i| (i as f32 * 0.05).sin()).collect();
        let mut buf = original.clone();
        eq.process_interleaved(&mut buf, 2);
        assert_eq!(buf, original, "a flat EQ must be bit-transparent");
    }

    #[test]
    fn bass_boost_lifts_lows_and_leaves_highs_alone() {
        let fs = 48_000;
        let mut eq = Equalizer::new(fs, 1);
        let mut gains = [0.0f32; BANDS];
        gains[0] = 6.0; // 31 Hz low shelf
        eq.set_params(&gains, 0.0, true);

        // A shelf reaches its full gain below the corner, half of it at the
        // corner, and tapers off above — assert that shape, not a flat boost.
        let deep = gain_at(&mut eq, fs, 15.0);
        let corner = gain_at(&mut eq, fs, 31.0);
        let high = gain_at(&mut eq, fs, 8_000.0);
        assert!(deep > 4.5, "15 Hz should approach +6 dB, got {deep:.2} dB");
        assert!(
            (corner - 3.0).abs() < 1.0,
            "31 Hz should sit near +3 dB, got {corner:.2} dB"
        );
        assert!(high.abs() < 0.5, "8 kHz should be untouched, got {high:.2} dB");
    }

    #[test]
    fn full_bass_boost_preset_gives_a_solid_low_end_lift() {
        // The preset stacks the lowest four bands; together they should lift
        // real bass content, not just subsonics.
        let fs = 48_000;
        let mut eq = Equalizer::new(fs, 1);
        let gains = [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        eq.set_params(&gains, 0.0, true);
        let bass = gain_at(&mut eq, fs, 80.0);
        assert!(bass > 3.0, "80 Hz should be clearly lifted, got {bass:.2} dB");
    }

    #[test]
    fn cutting_a_mid_band_attenuates_that_band() {
        let fs = 48_000;
        let mut eq = Equalizer::new(fs, 1);
        let mut gains = [0.0f32; BANDS];
        gains[5] = -12.0; // 1 kHz
        eq.set_params(&gains, 0.0, true);

        let at_band = gain_at(&mut eq, fs, 1_000.0);
        assert!(at_band < -6.0, "1 kHz should be cut, got {at_band:.2} dB");
    }

    #[test]
    fn preamp_scales_level() {
        let fs = 48_000;
        let mut eq = Equalizer::new(fs, 1);
        eq.set_params(&[0.0; BANDS], -6.0, true);
        let g = gain_at(&mut eq, fs, 1_000.0);
        assert!((g + 6.0).abs() < 0.2, "expected -6 dB, got {g:.2} dB");
    }

    #[test]
    fn disabled_equalizer_is_transparent() {
        let mut eq = Equalizer::new(48_000, 1);
        let mut gains = [0.0f32; BANDS];
        gains[0] = 12.0;
        eq.set_params(&gains, 6.0, false);
        let original: Vec<f32> = (0..256).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut buf = original.clone();
        eq.process_interleaved(&mut buf, 1);
        assert_eq!(buf, original);
    }

    #[test]
    fn bands_above_nyquist_stay_flat() {
        // 16 kHz band cannot exist at 22.05 kHz sample rate
        let mut eq = Equalizer::new(22_050, 1);
        let mut gains = [0.0f32; BANDS];
        gains[BANDS - 1] = 12.0;
        eq.set_params(&gains, 0.0, true);
        let mut buf: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.03).sin()).collect();
        eq.process_interleaved(&mut buf, 1);
        assert!(buf.iter().all(|v| v.is_finite()), "must not blow up");
    }
}
