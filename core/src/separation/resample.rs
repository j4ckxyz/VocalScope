//! Sample-rate conversion.
//!
//! The separation models work at 44.1 kHz. Recordings at any other rate are
//! converted with a polyphase windowed-sinc filter: the textbook method,
//! streamed, with the stop band about 90 dB down.

/// Input samples each output sample is computed from.
const TAPS: usize = 48;
const KAISER_BETA: f64 = 9.0;
/// Pass band as a share of the lower of the two Nyquist frequencies.
const PASSBAND: f64 = 0.94;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Zeroth-order modified Bessel function of the first kind.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..40 {
        term *= (half / k as f64).powi(2);
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

/// Converts interleaved audio from one sample rate to another.
pub struct Resampler {
    channels: usize,
    /// Output rate over input rate, in lowest terms: `up / down`.
    up: u64,
    down: u64,
    /// `table[phase * TAPS + tap]`.
    table: Vec<f32>,
    /// Per-channel input not yet consumed. `buffer[c][0]` is sample
    /// `buffer_start` of the stream, which begins with a short run of
    /// silence so the filter's delay is taken out.
    buffer: Vec<Vec<f32>>,
    buffer_start: u64,
    /// Index of the next output sample.
    next: u64,
    frames_in: u64,
}

impl Resampler {
    /// `None` when no conversion is needed.
    pub fn new(from_hz: u32, to_hz: u32, channels: usize) -> Option<Self> {
        if from_hz == to_hz || from_hz == 0 || to_hz == 0 {
            return None;
        }
        let divisor = gcd(from_hz as u64, to_hz as u64);
        let up = to_hz as u64 / divisor;
        let down = from_hz as u64 / divisor;

        // One low-pass prototype at the common multiple of both rates, cut
        // off just below the lower Nyquist, split into `up` phases.
        let length = TAPS * up as usize;
        let cutoff = 0.5 * PASSBAND / up.max(down) as f64;
        // Centred on a whole sample, so the delay is exactly TAPS / 2 input
        // samples and can be taken out exactly.
        let centre = length as f64 / 2.0;
        let normaliser = bessel_i0(KAISER_BETA);
        let prototype: Vec<f64> = (0..length)
            .map(|m| {
                let x = m as f64 - centre;
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (std::f64::consts::TAU * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let position = x / centre;
                let window = bessel_i0(KAISER_BETA * (1.0 - position * position).max(0.0).sqrt())
                    / normaliser;
                sinc * window * up as f64
            })
            .collect();
        let mut table = vec![0f32; length];
        for phase in 0..up as usize {
            for tap in 0..TAPS {
                table[phase * TAPS + tap] =
                    prototype[phase + (TAPS - 1 - tap) * up as usize] as f32;
            }
        }
        Some(Self {
            channels: channels.max(1),
            up,
            down,
            table,
            buffer: vec![vec![0.0; TAPS / 2 - 1]; channels.max(1)],
            buffer_start: 0,
            next: 0,
            frames_in: 0,
        })
    }

    /// Feeds interleaved input; appends interleaved output to `out`.
    pub fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        for frame in interleaved.chunks_exact(self.channels) {
            for (channel, sample) in self.buffer.iter_mut().zip(frame) {
                channel.push(*sample);
            }
        }
        self.frames_in += (interleaved.len() / self.channels) as u64;
        self.produce(out, None);
    }

    /// Flushes the tail. The total output is the input length scaled by the
    /// rate ratio, rounded up.
    pub fn finish(mut self, out: &mut Vec<f32>) {
        let total = (self.frames_in * self.up).div_ceil(self.down);
        for channel in &mut self.buffer {
            channel.extend(std::iter::repeat_n(0.0, TAPS + 1));
        }
        self.produce(out, Some(total));
    }

    fn produce(&mut self, out: &mut Vec<f32>, limit: Option<u64>) {
        let available = self.buffer_start + self.buffer[0].len() as u64;
        loop {
            if limit.is_some_and(|limit| self.next >= limit) {
                break;
            }
            let position = self.next * self.down;
            let index = position / self.up;
            if index + TAPS as u64 > available {
                break;
            }
            let phase = (position % self.up) as usize;
            let taps = &self.table[phase * TAPS..(phase + 1) * TAPS];
            let at = (index - self.buffer_start) as usize;
            for channel in &self.buffer {
                let window = &channel[at..at + TAPS];
                out.push(window.iter().zip(taps).map(|(x, h)| x * h).sum());
            }
            self.next += 1;
        }
        // Drop input that no later output can reach.
        let keep_from = ((self.next * self.down / self.up).saturating_sub(self.buffer_start)
            as usize)
            .min(self.buffer[0].len());
        if keep_from > 4_096 {
            for channel in &mut self.buffer {
                channel.drain(..keep_from);
            }
            self.buffer_start += keep_from as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;

    fn sine(rate: u32, hz: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| (0.5 * (TAU * hz * n as f64 / rate as f64).sin()) as f32)
            .collect()
    }

    fn convert(from: u32, to: u32, input: &[f32], channels: usize) -> Vec<f32> {
        let mut resampler = Resampler::new(from, to, channels).unwrap();
        let mut out = Vec::new();
        for block in input.chunks(channels * 1_000 + channels) {
            resampler.push(block, &mut out);
        }
        resampler.finish(&mut out);
        out
    }

    #[test]
    fn no_resampler_is_needed_for_equal_rates() {
        assert!(Resampler::new(44_100, 44_100, 2).is_none());
        assert!(Resampler::new(0, 44_100, 2).is_none());
    }

    #[test]
    fn tones_keep_their_frequency_level_and_timing() {
        for from in [48_000, 96_000, 32_000, 22_050, 8_000] {
            // Well inside the pass band of every rate tried.
            let hz = 1_000.0;
            let out = convert(from, 44_100, &sine(from, hz, from as usize), 1);
            assert_eq!(out.len(), 44_100, "from {from}");
            let expected = sine(44_100, hz, 44_100);
            // Compare away from the very ends, where the filter runs off
            // the edge of the signal.
            let worst = out[200..43_900]
                .iter()
                .zip(&expected[200..43_900])
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(worst < 2e-3, "from {from}: worst error {worst}");
        }
    }

    #[test]
    fn frequencies_above_the_new_nyquist_are_removed() {
        // 30 kHz cannot exist at 44.1 kHz; left in, it would fold down to 14.1 kHz.
        let out = convert(96_000, 44_100, &sine(96_000, 30_000.0, 96_000), 1);
        let peak = out[500..43_600].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak < 1e-4, "alias level {peak}");
    }

    #[test]
    fn channels_stay_separate() {
        let left = sine(48_000, 440.0, 4_800);
        let interleaved: Vec<f32> = left.iter().flat_map(|s| [*s, 0.0]).collect();
        let out = convert(48_000, 44_100, &interleaved, 2);
        assert_eq!(out.len(), 2 * 4_410);
        assert!(out.chunks(2).all(|frame| frame[1] == 0.0));
        assert!(out.chunks(2).any(|frame| frame[0].abs() > 0.4));
    }

    #[test]
    fn lengths_round_up_and_empty_input_is_empty() {
        assert_eq!(convert(48_000, 44_100, &vec![0.0; 1_001], 1).len(), 920);
        assert!(convert(48_000, 44_100, &[], 2).is_empty());
    }
}
