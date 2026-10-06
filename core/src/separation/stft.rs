//! Short-time Fourier transform in the exact form the MDX-Net models were
//! trained with: a periodic Hann window, frames centred by reflecting the
//! signal at both ends, no normalisation going forward and a window-squared
//! overlap-add coming back.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

pub struct Stft {
    pub n_fft: usize,
    pub hop: usize,
    /// Frequency bins kept (the model ignores the top of the spectrum).
    pub dim_f: usize,
    /// Frames per chunk.
    pub dim_t: usize,
    window: Vec<f32>,
    /// Sum of squared windows at each sample of a padded chunk.
    envelope: Vec<f32>,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    padded: Vec<f32>,
    frame: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    accumulator: Vec<f32>,
}

impl Stft {
    pub fn new(n_fft: usize, hop: usize, dim_f: usize, dim_t: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(n_fft);
        let inverse = planner.plan_fft_inverse(n_fft);
        let window: Vec<f32> = (0..n_fft)
            .map(|n| (0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / n_fft as f64).cos()) as f32)
            .collect();
        let padded_len = hop * (dim_t - 1) + n_fft;
        let mut envelope = vec![0f32; padded_len];
        for t in 0..dim_t {
            for (n, w) in window.iter().enumerate() {
                envelope[t * hop + n] += w * w;
            }
        }
        Self {
            n_fft,
            hop,
            dim_f,
            dim_t,
            window,
            envelope,
            spectrum: forward.make_output_vec(),
            forward,
            inverse,
            padded: vec![0.0; padded_len],
            frame: vec![0.0; n_fft],
            accumulator: vec![0.0; padded_len],
        }
    }

    /// Samples per chunk.
    pub fn chunk_len(&self) -> usize {
        self.hop * (self.dim_t - 1)
    }

    /// Transforms one channel of one chunk. `real` and `imaginary` receive
    /// `dim_f × dim_t` values, frequency-major.
    pub fn forward(&mut self, samples: &[f32], real: &mut [f32], imaginary: &mut [f32]) {
        let half = self.n_fft / 2;
        let len = samples.len();
        // Reflect about the first and last samples, not repeating them.
        for i in 0..half {
            self.padded[i] = samples[half - i];
            self.padded[half + len + i] = samples[len - 2 - i];
        }
        self.padded[half..half + len].copy_from_slice(samples);

        for t in 0..self.dim_t {
            let start = t * self.hop;
            for ((out, sample), w) in self
                .frame
                .iter_mut()
                .zip(&self.padded[start..start + self.n_fft])
                .zip(&self.window)
            {
                *out = sample * w;
            }
            let _ = self.forward.process(&mut self.frame, &mut self.spectrum);
            for f in 0..self.dim_f {
                real[f * self.dim_t + t] = self.spectrum[f].re;
                imaginary[f * self.dim_t + t] = self.spectrum[f].im;
            }
        }
    }

    /// The inverse of [`Stft::forward`]; bins above `dim_f` are taken as zero.
    pub fn inverse(&mut self, real: &[f32], imaginary: &[f32], samples: &mut [f32]) {
        let half = self.n_fft / 2;
        let scale = 1.0 / self.n_fft as f32;
        self.accumulator.fill(0.0);
        for t in 0..self.dim_t {
            for f in 0..self.dim_f {
                self.spectrum[f] =
                    Complex::new(real[f * self.dim_t + t], imaginary[f * self.dim_t + t]);
            }
            self.spectrum[self.dim_f..].fill(Complex::new(0.0, 0.0));
            // A real signal has no imaginary part at zero frequency.
            self.spectrum[0].im = 0.0;
            let _ = self.inverse.process(&mut self.spectrum, &mut self.frame);
            let start = t * self.hop;
            for ((acc, sample), w) in self.accumulator[start..start + self.n_fft]
                .iter_mut()
                .zip(&self.frame)
                .zip(&self.window)
            {
                *acc += sample * scale * w;
            }
        }
        for (i, out) in samples.iter_mut().enumerate() {
            let weight = self.envelope[half + i];
            *out = if weight > 1e-8 {
                self.accumulator[half + i] / weight
            } else {
                0.0
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::pitch::test_support::noise;

    #[test]
    fn a_chunk_survives_the_round_trip_when_no_bins_are_dropped() {
        let (n_fft, hop, dim_t) = (512, 128, 32);
        let mut stft = Stft::new(n_fft, hop, n_fft / 2 + 1, dim_t);
        assert_eq!(stft.chunk_len(), 128 * 31);
        let samples = noise(stft.chunk_len(), 1);
        let size = stft.dim_f * dim_t;
        let (mut re, mut im) = (vec![0.0; size], vec![0.0; size]);
        stft.forward(&samples, &mut re, &mut im);
        let mut back = vec![0.0; samples.len()];
        stft.inverse(&re, &im, &mut back);
        let worst = samples
            .iter()
            .zip(&back)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-4, "worst error {worst}");
    }

    #[test]
    fn a_tone_lands_in_its_bin_with_the_expected_size() {
        let (n_fft, hop, dim_t) = (1024, 256, 16);
        let mut stft = Stft::new(n_fft, hop, 300, dim_t);
        // Exactly bin 64.
        let samples: Vec<f32> = (0..stft.chunk_len())
            .map(|n| (std::f64::consts::TAU * 64.0 * n as f64 / n_fft as f64).cos() as f32)
            .collect();
        let size = stft.dim_f * dim_t;
        let (mut re, mut im) = (vec![0.0; size], vec![0.0; size]);
        stft.forward(&samples, &mut re, &mut im);
        // A middle frame, unaffected by the reflected edges.
        let t = 8;
        let magnitude = |f: usize| (re[f * dim_t + t].powi(2) + im[f * dim_t + t].powi(2)).sqrt();
        // A unit cosine through a Hann window: half the window's sum, n_fft / 4.
        assert!((magnitude(64) - 256.0).abs() < 0.5, "{}", magnitude(64));
        assert!(magnitude(60) < 0.01 && magnitude(70) < 0.01);
    }

    #[test]
    fn dropping_the_top_bins_removes_only_high_frequencies() {
        let (n_fft, hop, dim_t) = (1024, 256, 16);
        let mut stft = Stft::new(n_fft, hop, 128, dim_t);
        let low: Vec<f32> = (0..stft.chunk_len())
            .map(|n| (std::f64::consts::TAU * 20.0 * n as f64 / n_fft as f64).sin() as f32)
            .collect();
        let high: Vec<f32> = (0..stft.chunk_len())
            .map(|n| (std::f64::consts::TAU * 300.0 * n as f64 / n_fft as f64).sin() as f32)
            .collect();
        let mixed: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
        let size = stft.dim_f * dim_t;
        let (mut re, mut im) = (vec![0.0; size], vec![0.0; size]);
        stft.forward(&mixed, &mut re, &mut im);
        let mut back = vec![0.0; mixed.len()];
        stft.inverse(&re, &im, &mut back);
        let worst = low[600..3_200]
            .iter()
            .zip(&back[600..3_200])
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-3, "worst error {worst}");
    }
}
