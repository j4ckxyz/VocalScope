//! Fundamental-frequency tracking.
//!
//! The tracker is YIN (de Cheveigné & Kawahara, 2002) with two changes that
//! matter for singing:
//!
//! * Every frame keeps several candidate periods instead of committing to
//!   one, and a Viterbi pass over the whole recording picks the path that is
//!   both periodic and continuous. That removes the isolated octave jumps a
//!   frame-by-frame tracker makes.
//! * The period is refined on the raw difference function at the recording's
//!   own sample rate, which keeps the error on steady tones well under a
//!   cent (see the tests; that precision is what the correction indicators
//!   rest on).
//!
//! Audio is consumed as a stream. Only a few seconds of mono samples are held
//! at a time, plus a handful of numbers per 10 ms frame.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Spacing of pitch frames.
pub const HOP_SECONDS: f64 = 0.01;
/// Lowest and highest fundamental the tracker reports. The range covers
/// every singing voice (roughly B1 to D6) with a margin.
pub const MIN_FREQUENCY_HZ: f64 = 60.0;
pub const MAX_FREQUENCY_HZ: f64 = 1200.0;

/// Length of the window that is compared against its shifted copy.
const WINDOW_SECONDS: f64 = 0.032;
/// Recordings above this rate are reduced by a whole factor first; nothing a
/// voice's pitch depends on lives up there and the work scales with the rate.
const MAX_ANALYSIS_RATE_HZ: u32 = 50_000;
const DECIMATED_RATE_FLOOR_HZ: u32 = 32_000;
/// Rumble and DC below this would otherwise dominate the difference function.
const HIGH_PASS_HZ: f64 = 45.0;

/// Candidate periods kept per frame.
const CANDIDATES: usize = 5;
/// A dip in the normalised difference function shallower than this is not a
/// pitch candidate at all.
const CANDIDATE_CEILING: f32 = 0.75;
/// Cost of calling a frame unvoiced. A voiced candidate has to be more
/// periodic than this to win.
const UNVOICED_COST: f32 = 0.32;
/// Cost of switching between voiced and unvoiced, which stops single-frame
/// flicker at the edges of notes.
const VOICING_SWITCH_COST: f32 = 0.12;
/// Cost per semitone of movement between consecutive frames, and the most a
/// single jump can cost.
const SEMITONE_COST: f32 = 0.035;
const MAX_JUMP_SEMITONES: f32 = 14.0;
/// A period twice as long fits a periodic signal equally well. Each octave
/// below the first good dip costs this much, which is what keeps the track on
/// the fundamental rather than a subharmonic.
const SUBHARMONIC_COST_PER_OCTAVE: f32 = 0.06;
/// Candidates within this much of the frame's best dip count as "good" when
/// finding the first one.
const FIRST_DIP_MARGIN: f32 = 0.1;
/// How much deeper a later dip must be to displace an earlier candidate.
const REPLACEMENT_MARGIN: f32 = 0.05;
/// Frames quieter than this, absolutely or relative to the loudest frame,
/// are unvoiced whatever their periodicity.
const ABSOLUTE_GATE_DB: f32 = -66.0;
const RELATIVE_GATE_DB: f32 = -54.0;
/// Frames analysed per batch; a batch is split across threads.
const BATCH_FRAMES: usize = 1_024;

/// A recording's pitch over time, one value per [`HOP_SECONDS`]. Frame `i`
/// describes the audio centred on `i * hop_seconds`.
#[derive(Debug, Clone, PartialEq)]
pub struct PitchTrack {
    pub hop_seconds: f64,
    /// Fundamental frequency in hertz; `0.0` where there is no pitch.
    pub frequency_hz: Vec<f32>,
    /// How periodic the frame is, 0–1. Meaningful for voiced frames.
    pub confidence: Vec<f32>,
    /// Level of the frame in dBFS (RMS).
    pub level_db: Vec<f32>,
}

impl PitchTrack {
    pub fn len(&self) -> usize {
        self.frequency_hz.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frequency_hz.is_empty()
    }

    pub fn duration_seconds(&self) -> f64 {
        self.len() as f64 * self.hop_seconds
    }

    /// Pitch as a fractional MIDI note number (69 = A4 = 440 Hz), or `None`
    /// where the frame is unvoiced or out of range.
    pub fn midi(&self, frame: usize) -> Option<f32> {
        let hz = *self.frequency_hz.get(frame)?;
        (hz > 0.0).then(|| hz_to_midi(hz))
    }

    pub fn voiced_frames(&self) -> usize {
        self.frequency_hz.iter().filter(|hz| **hz > 0.0).count()
    }
}

pub fn hz_to_midi(hz: f32) -> f32 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

pub fn midi_to_hz(midi: f32) -> f32 {
    440.0 * 2f32.powf((midi - 69.0) / 12.0)
}

/// What one frame could be, before the path through the recording is chosen.
#[derive(Clone, Copy)]
struct Frame {
    level_db: f32,
    count: u8,
    /// Candidate periods in samples (fractional).
    period: [f32; CANDIDATES],
    /// Normalised difference at each candidate: 0 is perfectly periodic.
    aperiodicity: [f32; CANDIDATES],
}

/// Per-thread working storage for the difference function.
struct FrameAnalyzer {
    window: usize,
    min_period: usize,
    max_period: usize,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    head: Vec<f32>,
    whole: Vec<f32>,
    head_spectrum: Vec<Complex<f32>>,
    whole_spectrum: Vec<Complex<f32>>,
    correlation: Vec<f32>,
    difference: Vec<f32>,
    normalised: Vec<f32>,
    energy: Vec<f64>,
}

impl FrameAnalyzer {
    fn new(geometry: &Geometry, planner: &mut RealFftPlanner<f32>) -> Self {
        let size = geometry.fft_size;
        let forward = planner.plan_fft_forward(size);
        let inverse = planner.plan_fft_inverse(size);
        Self {
            window: geometry.window,
            min_period: geometry.min_period,
            max_period: geometry.max_period,
            head: vec![0.0; size],
            whole: vec![0.0; size],
            head_spectrum: forward.make_output_vec(),
            whole_spectrum: forward.make_output_vec(),
            correlation: vec![0.0; size],
            difference: vec![0.0; geometry.max_period + 1],
            normalised: vec![0.0; geometry.max_period + 1],
            energy: vec![0.0; geometry.frame_len + 1],
            forward,
            inverse,
        }
    }

    /// `samples` is `window + max_period` long.
    fn analyse(&mut self, samples: &[f32]) -> Frame {
        let window = self.window;
        let size = self.head.len();

        // Prefix sums of energy give every shifted window's energy for free.
        let mut total = 0f64;
        self.energy[0] = 0.0;
        for (i, sample) in samples.iter().enumerate() {
            total += (*sample as f64) * (*sample as f64);
            self.energy[i + 1] = total;
        }
        let head_energy = self.energy[window];
        let rms = (head_energy / window as f64).sqrt();
        let level_db = if rms > 0.0 {
            (20.0 * rms.log10()) as f32
        } else {
            f32::NEG_INFINITY
        };
        let mut frame = Frame {
            level_db,
            count: 0,
            period: [0.0; CANDIDATES],
            aperiodicity: [1.0; CANDIDATES],
        };
        if !(level_db > ABSOLUTE_GATE_DB) {
            return frame;
        }

        // r(τ) = Σ x[j]·x[j+τ] for j in the first window, as a correlation of
        // that window with the whole frame, computed in the frequency domain.
        self.head[..window].copy_from_slice(&samples[..window]);
        self.head[window..].fill(0.0);
        self.whole[..samples.len()].copy_from_slice(samples);
        self.whole[samples.len()..].fill(0.0);
        // The planner's own scratch handling is fine here; errors only arise
        // from mismatched buffer lengths, which are fixed at construction.
        let _ = self
            .forward
            .process(&mut self.head, &mut self.head_spectrum);
        let _ = self
            .forward
            .process(&mut self.whole, &mut self.whole_spectrum);
        for (a, b) in self.head_spectrum.iter_mut().zip(&self.whole_spectrum) {
            *a = a.conj() * b;
        }
        let _ = self
            .inverse
            .process(&mut self.head_spectrum, &mut self.correlation);
        let scale = 1.0 / size as f64;

        // d(τ) = Σ (x[j] − x[j+τ])², then YIN's cumulative-mean normalisation.
        let mut running = 0f64;
        self.difference[0] = 0.0;
        self.normalised[0] = 1.0;
        for tau in 1..=self.max_period {
            let shifted_energy = self.energy[tau + window] - self.energy[tau];
            let value = (head_energy + shifted_energy - 2.0 * self.correlation[tau] as f64 * scale)
                .max(0.0);
            running += value;
            self.difference[tau] = value as f32;
            self.normalised[tau] = if running > 0.0 {
                (value * tau as f64 / running) as f32
            } else {
                1.0
            };
        }

        // Every dip is a candidate; keep the deepest few.
        let mut found: [(f32, usize); CANDIDATES] = [(f32::MAX, 0); CANDIDATES];
        let mut count = 0usize;
        for tau in self.min_period.max(2)..self.max_period {
            let here = self.normalised[tau];
            if here >= CANDIDATE_CEILING
                || here >= self.normalised[tau - 1]
                || here > self.normalised[tau + 1]
            {
                continue;
            }
            if count < CANDIDATES {
                found[count] = (here, tau);
                count += 1;
            } else if let Some(worst) = (0..CANDIDATES)
                .max_by(|a, b| found[*a].0.total_cmp(&found[*b].0))
                // A high note has a dip at every multiple of its period, all
                // about equally deep. Earlier dips are only displaced by
                // clearly deeper ones, so the true period is never crowded
                // out by its own subharmonics.
                .filter(|worst| here < found[*worst].0 - REPLACEMENT_MARGIN)
            {
                found[worst] = (here, tau);
            }
        }
        found[..count].sort_by_key(|(_, tau)| *tau);
        for (slot, (value, tau)) in found[..count].iter().enumerate() {
            frame.period[slot] = *tau as f32 + parabolic_offset(&self.difference, *tau);
            frame.aperiodicity[slot] = *value;
        }
        frame.count = count as u8;
        frame
    }
}

/// Sub-sample position of a minimum, from the parabola through it and its
/// neighbours. Taken on the raw difference function, whose minimum is not
/// skewed by the normalisation.
fn parabolic_offset(values: &[f32], index: usize) -> f32 {
    let (before, here, after) = (values[index - 1], values[index], values[index + 1]);
    let curvature = before - 2.0 * here + after;
    if curvature <= 0.0 {
        return 0.0;
    }
    (0.5 * (before - after) / curvature).clamp(-1.0, 1.0)
}

#[derive(Clone, Copy)]
struct Geometry {
    rate: f64,
    window: usize,
    min_period: usize,
    max_period: usize,
    frame_len: usize,
    fft_size: usize,
}

impl Geometry {
    fn new(rate: f64) -> Self {
        let window = (WINDOW_SECONDS * rate).round().max(16.0) as usize;
        let max_period = (rate / MIN_FREQUENCY_HZ).ceil() as usize + 1;
        let min_period = ((rate / MAX_FREQUENCY_HZ).floor() as usize).max(2);
        let frame_len = window + max_period;
        Self {
            rate,
            window,
            min_period,
            max_period,
            frame_len,
            fft_size: frame_len.next_power_of_two(),
        }
    }
}

/// Second-order Butterworth high-pass.
struct HighPass {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl HighPass {
    fn new(cutoff_hz: f64, rate: f64) -> Self {
        let k = (std::f64::consts::PI * cutoff_hz / rate).tan();
        let q = std::f64::consts::FRAC_1_SQRT_2;
        let norm = 1.0 / (1.0 + k / q + k * k);
        Self {
            b: [norm, -2.0 * norm, norm],
            a: [2.0 * (k * k - 1.0) * norm, (1.0 - k / q + k * k) * norm],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn step(&mut self, input: f64) -> f64 {
        let output = self.b[0] * input + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [input, self.x[0]];
        self.y = [output, self.y[0]];
        output
    }
}

/// Low-pass filter and keep every `factor`-th sample.
struct Decimator {
    factor: usize,
    taps: Vec<f32>,
    history: Vec<f32>,
    /// Input samples still to skip before the next output.
    phase: usize,
}

impl Decimator {
    fn new(factor: usize) -> Self {
        // Windowed sinc with its cutoff at 90% of the new Nyquist frequency.
        let half = 16 * factor;
        let cutoff = 0.45 / factor as f64;
        let mut taps: Vec<f32> = (0..=2 * half)
            .map(|n| {
                let x = n as f64 - half as f64;
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (std::f64::consts::TAU * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let w = std::f64::consts::TAU * n as f64 / (2 * half) as f64;
                let blackman = 0.42 - 0.5 * w.cos() + 0.08 * (2.0 * w).cos();
                (sinc * blackman) as f32
            })
            .collect();
        let gain: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|tap| *tap /= gain);
        Self {
            factor,
            history: vec![0.0; taps.len() - 1],
            taps,
            phase: 0,
        }
    }

    fn push(&mut self, input: &[f32], output: &mut Vec<f32>) {
        let taps = self.taps.len();
        self.history.extend_from_slice(input);
        let available = self.history.len().saturating_sub(taps - 1);
        let mut at = self.phase;
        while at < available {
            let window = &self.history[at..at + taps];
            output.push(window.iter().zip(&self.taps).map(|(x, h)| x * h).sum());
            at += self.factor;
        }
        self.phase = at - available;
        self.history.drain(..available);
    }
}

/// Builds a [`PitchTrack`] from interleaved sample blocks.
pub struct PitchAnalyzer {
    channels: usize,
    geometry: Geometry,
    high_pass: HighPass,
    decimator: Option<Decimator>,
    mono: Vec<f32>,
    /// Analysis-rate samples not yet fully consumed. `pending[0]` is sample
    /// `pending_start` of the stream, which begins with half a window of
    /// silence so that frame 0 is centred on time zero.
    pending: Vec<f32>,
    pending_start: usize,
    /// Real samples received (excluding the leading silence).
    samples_seen: u64,
    frames: Vec<Frame>,
    workers: Vec<FrameAnalyzer>,
}

impl PitchAnalyzer {
    pub fn new(sample_rate_hz: u32, channel_count: u16) -> Self {
        let factor = if sample_rate_hz > MAX_ANALYSIS_RATE_HZ {
            (sample_rate_hz / DECIMATED_RATE_FLOOR_HZ).max(2) as usize
        } else {
            1
        };
        let rate = sample_rate_hz.max(1) as f64 / factor as f64;
        let geometry = Geometry::new(rate);
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 4);
        let mut planner = RealFftPlanner::<f32>::new();
        Self {
            channels: channel_count.max(1) as usize,
            geometry,
            high_pass: HighPass::new(HIGH_PASS_HZ, rate),
            decimator: (factor > 1).then(|| Decimator::new(factor)),
            mono: Vec::new(),
            pending: vec![0.0; geometry.window / 2],
            pending_start: 0,
            samples_seen: 0,
            frames: Vec::new(),
            workers: (0..threads)
                .map(|_| FrameAnalyzer::new(&geometry, &mut planner))
                .collect(),
        }
    }

    /// Sample offset, in the padded stream, at which frame `index` starts.
    fn frame_start(&self, index: usize) -> usize {
        (index as f64 * HOP_SECONDS * self.geometry.rate).round() as usize
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        self.mono.clear();
        let scale = 1.0 / self.channels as f32;
        self.mono.extend(
            interleaved
                .chunks_exact(self.channels)
                .map(|frame| frame.iter().sum::<f32>() * scale),
        );
        let before = self.pending.len();
        match &mut self.decimator {
            Some(decimator) => decimator.push(&self.mono, &mut self.pending),
            None => self.pending.extend_from_slice(&self.mono),
        }
        for sample in &mut self.pending[before..] {
            *sample = self.high_pass.step(*sample as f64) as f32;
        }
        self.samples_seen += (self.pending.len() - before) as u64;
        self.run(BATCH_FRAMES);
    }

    /// Analyses every frame that is completely buffered, at least
    /// `minimum` at a time.
    fn run(&mut self, minimum: usize) {
        loop {
            let first = self.frames.len();
            let buffered_end = self.pending_start + self.pending.len();
            // Largest count of frames whose samples are all present.
            let mut ready = 0usize;
            while ready < BATCH_FRAMES
                && self.frame_start(first + ready) + self.geometry.frame_len <= buffered_end
            {
                ready += 1;
            }
            if ready == 0 || ready < minimum {
                return;
            }

            let starts: Vec<usize> = (0..ready)
                .map(|i| self.frame_start(first + i) - self.pending_start)
                .collect();
            let frame_len = self.geometry.frame_len;
            let pending = &self.pending;
            let mut results = vec![
                Frame {
                    level_db: f32::NEG_INFINITY,
                    count: 0,
                    period: [0.0; CANDIDATES],
                    aperiodicity: [1.0; CANDIDATES],
                };
                ready
            ];
            let share = ready.div_ceil(self.workers.len());
            std::thread::scope(|scope| {
                for ((worker, starts), out) in self
                    .workers
                    .iter_mut()
                    .zip(starts.chunks(share))
                    .zip(results.chunks_mut(share))
                {
                    scope.spawn(move || {
                        for (start, slot) in starts.iter().zip(out) {
                            *slot = worker.analyse(&pending[*start..*start + frame_len]);
                        }
                    });
                }
            });
            self.frames.extend(results);

            let keep_from = self.frame_start(self.frames.len()) - self.pending_start;
            self.pending.drain(..keep_from.min(self.pending.len()));
            self.pending_start += keep_from;
        }
    }

    /// Frames analysed so far, for progress reporting.
    pub fn frames_done(&self) -> usize {
        self.frames.len()
    }

    pub fn finish(mut self) -> PitchTrack {
        // One frame per hop of real audio; pad so the last ones are complete.
        let duration = self.samples_seen as f64 / self.geometry.rate;
        let total = (duration / HOP_SECONDS).ceil() as usize;
        if total > 0 {
            let needed = self.frame_start(total - 1) + self.geometry.frame_len;
            let have = self.pending_start + self.pending.len();
            self.pending
                .extend(std::iter::repeat_n(0.0, needed.saturating_sub(have)));
            self.run(1);
        }
        self.frames.truncate(total);
        decode_path(&self.frames, self.geometry.rate)
    }
}

/// Chooses the most plausible sequence of candidates (or "unvoiced") through
/// the whole recording.
fn decode_path(frames: &[Frame], rate: f64) -> PitchTrack {
    const STATES: usize = CANDIDATES + 1;
    const UNVOICED: usize = CANDIDATES;
    let count = frames.len();
    let loudest = frames
        .iter()
        .map(|f| f.level_db)
        .fold(f32::NEG_INFINITY, f32::max);
    let gate = ABSOLUTE_GATE_DB.max(loudest + RELATIVE_GATE_DB);

    // Semitone position of every candidate, and the cost of choosing it.
    let mut pitch = vec![[0f32; CANDIDATES]; count];
    let mut emission = vec![[f32::INFINITY; STATES]; count];
    for (i, frame) in frames.iter().enumerate() {
        emission[i][UNVOICED] = UNVOICED_COST;
        let n = frame.count as usize;
        if n == 0 || frame.level_db < gate {
            continue;
        }
        let best = frame.aperiodicity[..n]
            .iter()
            .copied()
            .fold(f32::MAX, f32::min);
        // Candidates are ordered by period, so this is the highest pitch
        // that fits about as well as anything does.
        let first_good = (0..n)
            .find(|c| frame.aperiodicity[*c] <= best + FIRST_DIP_MARGIN)
            .unwrap_or(0);
        for c in 0..n {
            pitch[i][c] = 12.0 * (rate as f32 / frame.period[c]).log2();
            let octaves_below = (frame.period[c] / frame.period[first_good]).log2().max(0.0);
            emission[i][c] = frame.aperiodicity[c] + SUBHARMONIC_COST_PER_OCTAVE * octaves_below;
        }
    }

    let mut cost = [f32::INFINITY; STATES];
    let mut back = vec![[0u8; STATES]; count];
    if let Some(first) = emission.first() {
        cost = *first;
    }
    for i in 1..count {
        let mut next = [f32::INFINITY; STATES];
        for to in 0..STATES {
            if !emission[i][to].is_finite() {
                continue;
            }
            let mut best = f32::INFINITY;
            let mut from_best = 0u8;
            for from in 0..STATES {
                if !cost[from].is_finite() {
                    continue;
                }
                let step = match (from == UNVOICED, to == UNVOICED) {
                    (true, true) => 0.0,
                    (true, false) | (false, true) => VOICING_SWITCH_COST,
                    (false, false) => {
                        SEMITONE_COST
                            * (pitch[i][to] - pitch[i - 1][from])
                                .abs()
                                .min(MAX_JUMP_SEMITONES)
                    }
                };
                let total = cost[from] + step;
                if total < best {
                    best = total;
                    from_best = from as u8;
                }
            }
            next[to] = best + emission[i][to];
            back[i][to] = from_best;
        }
        cost = next;
    }

    let mut track = PitchTrack {
        hop_seconds: HOP_SECONDS,
        frequency_hz: vec![0.0; count],
        confidence: vec![0.0; count],
        level_db: frames.iter().map(|f| f.level_db).collect(),
    };
    if count == 0 {
        return track;
    }
    let mut state = (0..STATES)
        .min_by(|a, b| cost[*a].total_cmp(&cost[*b]))
        .unwrap_or(UNVOICED);
    for i in (0..count).rev() {
        let frame = &frames[i];
        if state != UNVOICED {
            let hz = (rate / frame.period[state] as f64) as f32;
            if (MIN_FREQUENCY_HZ as f32..=MAX_FREQUENCY_HZ as f32).contains(&hz) {
                track.frequency_hz[i] = hz;
                track.confidence[i] = (1.0 - frame.aperiodicity[state]).clamp(0.0, 1.0);
            }
        } else if frame.count > 0 {
            // Still worth recording how periodic the rejected frame was.
            let best = frame.aperiodicity[..frame.count as usize]
                .iter()
                .copied()
                .fold(f32::MAX, f32::min);
            track.confidence[i] = (1.0 - best).clamp(0.0, 1.0);
        }
        state = back[i][state] as usize;
    }
    track
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::f64::consts::TAU;

    use super::*;

    /// Mono samples of a harmonic tone whose pitch (fractional MIDI) and
    /// loudness (0–1) are given per sample by the closures.
    pub fn synth(
        rate: u32,
        seconds: f64,
        harmonics: &[f64],
        midi_at: impl Fn(f64) -> f64,
        gain_at: impl Fn(f64) -> f64,
    ) -> Vec<f32> {
        let mut phase = 0f64;
        (0..(seconds * rate as f64) as usize)
            .map(|n| {
                let t = n as f64 / rate as f64;
                let hz = 440.0 * 2f64.powf((midi_at(t) - 69.0) / 12.0);
                phase += TAU * hz / rate as f64;
                let sample: f64 = harmonics
                    .iter()
                    .enumerate()
                    .map(|(k, level)| level * ((k + 1) as f64 * phase).sin())
                    .sum();
                (sample * gain_at(t)) as f32
            })
            .collect()
    }

    pub fn track(rate: u32, samples: &[f32]) -> PitchTrack {
        let mut analyzer = PitchAnalyzer::new(rate, 1);
        // Irregular block sizes, as a decoder would deliver.
        for block in samples.chunks(4_097) {
            analyzer.push(block);
        }
        analyzer.finish()
    }

    /// Deterministic white noise in ±1.
    pub fn noise(count: usize, seed: u64) -> Vec<f32> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..count)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    const VOICE: [f64; 6] = [1.0, 0.6, 0.45, 0.3, 0.2, 0.12];

    fn cents(a: f32, b: f64) -> f64 {
        1200.0 * (a as f64 / b).log2()
    }

    /// Largest error, in cents, over the settled middle of a steady tone.
    fn steady_error(rate: u32, hz: f64, harmonics: &[f64]) -> f64 {
        let midi = 69.0 + 12.0 * (hz / 440.0).log2();
        let samples = synth(rate, 1.0, harmonics, |_| midi, |_| 0.5);
        let track = track(rate, &samples);
        assert_eq!(track.len(), 100);
        track.frequency_hz[10..90]
            .iter()
            .map(|f| {
                assert!(*f > 0.0, "{hz} Hz at {rate} Hz was not tracked");
                cents(*f, hz).abs()
            })
            .fold(0.0, f64::max)
    }

    #[test]
    fn steady_tones_are_tracked_to_a_fraction_of_a_cent() {
        let mut worst = 0f64;
        for rate in [44_100, 48_000] {
            for hz in [
                65.4, 82.4, 110.0, 196.0, 261.63, 440.0, 523.25, 698.46, 987.77, 1108.7,
            ] {
                worst = worst.max(steady_error(rate, hz, &[1.0]));
                worst = worst.max(steady_error(rate, hz, &VOICE));
            }
        }
        assert!(worst < 0.5, "worst error was {worst:.3} cents");
    }

    #[test]
    fn other_sample_rates_stay_within_a_few_cents() {
        for rate in [16_000, 22_050, 32_000, 88_200, 96_000, 192_000] {
            for hz in [110.0, 261.63, 440.0, 880.0] {
                let error = steady_error(rate, hz, &VOICE);
                assert!(error < 3.0, "{hz} Hz at {rate} Hz: {error:.2} cents");
            }
        }
    }

    #[test]
    fn follows_vibrato_and_glides() {
        let rate = 44_100;
        // A4 with 6 Hz vibrato of ±40 cents, then a glide up to C5.
        let midi_at = |t: f64| {
            let base = if t < 1.0 {
                69.0
            } else {
                69.0 + 3.0 * ((t - 1.0) / 0.15).min(1.0)
            };
            base + 0.4 * (std::f64::consts::TAU * 6.0 * t).sin()
        };
        let samples = synth(rate, 2.0, &VOICE, midi_at, |_| 0.4);
        let track = track(rate, &samples);
        let mut errors: Vec<f64> = (10..190)
            .map(|i| {
                let expected = midi_at(i as f64 * HOP_SECONDS);
                (track.midi(i).expect("voiced") as f64 - expected).abs() * 100.0
            })
            .collect();
        errors.sort_by(f64::total_cmp);
        let median = errors[errors.len() / 2];
        let worst = errors[errors.len() - 1];
        // Each estimate averages over the window plus one period (about
        // 34 ms here), which rounds the peaks of the vibrato off slightly.
        assert!(median < 2.5, "median error {median:.2} cents");
        assert!(worst < 35.0, "worst error {worst:.2} cents");
    }

    #[test]
    fn silence_and_noise_are_unvoiced() {
        let rate = 44_100;
        let silent = track(rate, &vec![0.0; rate as usize]);
        assert_eq!(silent.len(), 100);
        assert_eq!(silent.voiced_frames(), 0);

        let hiss: Vec<f32> = noise(rate as usize * 2, 7)
            .iter()
            .map(|s| s * 0.3)
            .collect();
        let hiss = track(rate, &hiss);
        assert!(
            hiss.voiced_frames() <= 4,
            "{} of {} noise frames were called voiced",
            hiss.voiced_frames(),
            hiss.len()
        );
    }

    #[test]
    fn notes_separated_by_breaths_have_clean_edges() {
        let rate = 44_100;
        // 0.5 s of A3, 0.3 s of breath noise, 0.5 s of E4.
        let mut samples = synth(rate, 0.5, &VOICE, |_| 57.0, |_| 0.4);
        samples.extend(
            noise((rate as f64 * 0.3) as usize, 3)
                .iter()
                .map(|s| s * 0.02),
        );
        samples.extend(synth(rate, 0.5, &VOICE, |_| 64.0, |_| 0.4));
        let track = track(rate, &samples);

        for i in 3..47 {
            assert!((track.midi(i).unwrap() - 57.0).abs() < 0.02, "frame {i}");
        }
        for i in 53..77 {
            assert_eq!(track.frequency_hz[i], 0.0, "frame {i} should be unvoiced");
        }
        for i in 83..127 {
            assert!((track.midi(i).unwrap() - 64.0).abs() < 0.02, "frame {i}");
        }
    }

    #[test]
    fn stays_on_the_fundamental_when_it_is_weak_or_low() {
        let rate = 44_100;
        // A second harmonic four times stronger than the fundamental invites
        // an octave-up error; a rich low note invites an octave-down one.
        for (midi, harmonics) in [
            (45.0, vec![0.25, 1.0, 0.5, 0.25]),
            (38.0, vec![1.0, 0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3]),
            (76.0, vec![1.0, 0.05]),
        ] {
            let samples = synth(rate, 1.0, &harmonics, |_| midi, |_| 0.3);
            let track = track(rate, &samples);
            for i in 10..90 {
                let got = track.midi(i).unwrap_or(0.0) as f64;
                assert!(
                    (got - midi).abs() < 0.05,
                    "midi {midi}: frame {i} was {got}"
                );
            }
        }
    }

    #[test]
    fn a_voice_over_noise_is_still_tracked() {
        let rate = 44_100;
        let mut samples = synth(rate, 1.5, &VOICE, |_| 62.0, |_| 0.3);
        for (sample, hiss) in samples.iter_mut().zip(noise(rate as usize * 2, 11)) {
            *sample += hiss * 0.03;
        }
        let track = track(rate, &samples);
        let voiced = (10..140).filter(|i| track.frequency_hz[*i] > 0.0).count();
        assert!(voiced >= 128, "only {voiced} of 130 frames were voiced");
        for i in 10..140 {
            if let Some(midi) = track.midi(i) {
                assert!((midi - 62.0).abs() < 0.1, "frame {i} was {midi}");
            }
        }
    }

    #[test]
    fn stereo_and_block_boundaries_do_not_change_the_result() {
        let rate = 44_100;
        let mono = synth(rate, 1.0, &VOICE, |_| 60.0, |_| 0.4);
        let whole = track(rate, &mono);

        let stereo: Vec<f32> = mono.iter().flat_map(|s| [*s, *s]).collect();
        let mut analyzer = PitchAnalyzer::new(rate, 2);
        for block in stereo.chunks(2 * 333) {
            analyzer.push(block);
        }
        let chunked = analyzer.finish();
        assert_eq!(whole.len(), chunked.len());
        for (a, b) in whole.frequency_hz.iter().zip(&chunked.frequency_hz) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn an_empty_stream_yields_an_empty_track() {
        let track = PitchAnalyzer::new(44_100, 2).finish();
        assert!(track.is_empty());
        assert_eq!(track.duration_seconds(), 0.0);
    }

    #[test]
    fn converts_between_hertz_and_midi() {
        assert!((hz_to_midi(440.0) - 69.0).abs() < 1e-5);
        assert!((hz_to_midi(261.6256) - 60.0).abs() < 1e-4);
        assert!((midi_to_hz(57.0) - 220.0).abs() < 1e-3);
    }
}
