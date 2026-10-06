//! Multi-resolution min/max waveform summary.
//!
//! A recording is reduced, in one streaming pass, to a pyramid of min/max
//! buckets. The finest level covers [`BASE_FRAMES_PER_BUCKET`] frames per
//! bucket and each level above it merges [`LEVEL_FACTOR`] buckets, so any
//! zoom level can be drawn by touching only a few buckets per pixel. A
//! three-hour 44.1 kHz file needs roughly 40 MB for the whole pyramid.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::decode::AudioReader;
use super::StereoContent;
use crate::error::{AppError, AppResult};

pub const BASE_FRAMES_PER_BUCKET: u32 = 64;
const LEVEL_FACTOR: usize = 4;
/// When drawing, each output column is built from at least this many buckets
/// (unless zoomed in past the base resolution).
const MIN_BUCKETS_PER_COLUMN: usize = 4;
/// Stop adding coarser levels once a level is this small.
const MIN_LEVEL_LEN: usize = 512;
/// Two channels count as identical when the side (L−R) signal carries less
/// than this fraction of the total energy (−50 dB).
const DUAL_MONO_SIDE_ENERGY_RATIO: f64 = 1e-5;

const CACHE_MAGIC: &[u8; 4] = b"VSPK";
const CACHE_VERSION: u32 = 1;

type Bucket = [i16; 2];

#[derive(Debug, Clone, PartialEq)]
pub struct Peaks {
    sample_rate_hz: u32,
    channel_count: u16,
    total_frames: u64,
    stereo_content: StereoContent,
    /// Largest absolute sample value, 0.0–1.0 (can exceed 1.0 for float or
    /// lossy sources that overshoot).
    peak_amplitude: f32,
    /// `levels[0]` is the base resolution.
    levels: Vec<Vec<Bucket>>,
}

/// Facts about a recording that are only known after a full decode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
pub struct WaveformSummary {
    pub sample_rate_hz: u32,
    pub channel_count: u16,
    pub frame_count: u64,
    pub duration_seconds: f64,
    pub stereo_content: StereoContent,
    pub peak_amplitude: f32,
    pub peak_dbfs: Option<f32>,
}

impl Peaks {
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    pub fn duration_seconds(&self) -> f64 {
        self.total_frames as f64 / self.sample_rate_hz as f64
    }

    pub fn summary(&self) -> WaveformSummary {
        WaveformSummary {
            sample_rate_hz: self.sample_rate_hz,
            channel_count: self.channel_count,
            frame_count: self.total_frames,
            duration_seconds: self.duration_seconds(),
            stereo_content: self.stereo_content,
            peak_amplitude: self.peak_amplitude,
            peak_dbfs: (self.peak_amplitude > 0.0).then(|| 20.0 * self.peak_amplitude.log10()),
        }
    }

    /// Returns `buckets` min/max pairs (interleaved, `i16` full scale)
    /// covering the frame range `[start_frame, end_frame)`. Parts of the range
    /// beyond the end of the recording are returned as silence.
    pub fn query(&self, start_frame: u64, end_frame: u64, buckets: usize) -> Vec<i16> {
        let mut out = vec![0i16; buckets * 2];
        if buckets == 0 || end_frame <= start_frame || self.levels.is_empty() {
            return out;
        }
        let span = (end_frame - start_frame) as f64;
        let frames_per_out = span / buckets as f64;

        // Coarsest level that still gives at least `MIN_BUCKETS_PER_COLUMN`
        // buckets per output column. Level buckets rarely line up with column
        // edges, so a column also sees a sliver of its neighbours; keeping the
        // buckets small relative to a column keeps that bleed under a quarter
        // of a pixel.
        let mut level = 0usize;
        let mut level_frames = BASE_FRAMES_PER_BUCKET as f64;
        while level + 1 < self.levels.len()
            && level_frames * (LEVEL_FACTOR * MIN_BUCKETS_PER_COLUMN) as f64 <= frames_per_out
        {
            level += 1;
            level_frames *= LEVEL_FACTOR as f64;
        }
        let data = &self.levels[level];

        for i in 0..buckets {
            let from = start_frame as f64 + i as f64 * frames_per_out;
            if from >= self.total_frames as f64 {
                break;
            }
            let to = from + frames_per_out;
            let first = (from / level_frames).floor() as usize;
            let last = ((to / level_frames).ceil() as usize)
                .max(first + 1)
                .min(data.len());
            if first >= last {
                continue;
            }
            let mut min = i16::MAX;
            let mut max = i16::MIN;
            for bucket in &data[first..last] {
                min = min.min(bucket[0]);
                max = max.max(bucket[1]);
            }
            out[i * 2] = min;
            out[i * 2 + 1] = max;
        }
        out
    }

    /// Writes the base level to a compact cache file. Coarser levels are
    /// rebuilt on load.
    pub fn save(&self, path: &Path) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            w.write_all(CACHE_MAGIC)?;
            w.write_all(&CACHE_VERSION.to_le_bytes())?;
            w.write_all(&BASE_FRAMES_PER_BUCKET.to_le_bytes())?;
            w.write_all(&self.sample_rate_hz.to_le_bytes())?;
            w.write_all(&self.channel_count.to_le_bytes())?;
            w.write_all(&[stereo_content_to_byte(self.stereo_content)])?;
            w.write_all(&self.total_frames.to_le_bytes())?;
            w.write_all(&self.peak_amplitude.to_le_bytes())?;
            let base = &self.levels[0];
            w.write_all(&(base.len() as u64).to_le_bytes())?;
            for bucket in base {
                w.write_all(&bucket[0].to_le_bytes())?;
                w.write_all(&bucket[1].to_le_bytes())?;
            }
            w.flush()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Loads a cache file written by [`Peaks::save`]. Returns `None` when the
    /// file is missing, truncated or from an incompatible version, in which
    /// case the caller simply recomputes.
    pub fn load(path: &Path) -> Option<Self> {
        let mut r = BufReader::new(File::open(path).ok()?);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic).ok()?;
        if &magic != CACHE_MAGIC || read_u32(&mut r)? != CACHE_VERSION {
            return None;
        }
        if read_u32(&mut r)? != BASE_FRAMES_PER_BUCKET {
            return None;
        }
        let sample_rate_hz = read_u32(&mut r)?;
        let mut two = [0u8; 2];
        r.read_exact(&mut two).ok()?;
        let channel_count = u16::from_le_bytes(two);
        let mut one = [0u8; 1];
        r.read_exact(&mut one).ok()?;
        let stereo_content = stereo_content_from_byte(one[0])?;
        let total_frames = read_u64(&mut r)?;
        let mut four = [0u8; 4];
        r.read_exact(&mut four).ok()?;
        let peak_amplitude = f32::from_le_bytes(four);
        let len = read_u64(&mut r)? as usize;
        let expected = total_frames.div_ceil(BASE_FRAMES_PER_BUCKET as u64) as usize;
        if sample_rate_hz == 0 || len != expected {
            return None;
        }
        // Decode in small chunks straight into the final vector, so loading a
        // long recording never holds two copies of its summary in memory.
        let mut base: Vec<Bucket> = Vec::with_capacity(len);
        let mut chunk = [0u8; 16 * 1024];
        let mut remaining = len * 4;
        while remaining > 0 {
            let take = remaining.min(chunk.len());
            r.read_exact(&mut chunk[..take]).ok()?;
            base.extend(chunk[..take].chunks_exact(4).map(|c| {
                [
                    i16::from_le_bytes([c[0], c[1]]),
                    i16::from_le_bytes([c[2], c[3]]),
                ]
            }));
            remaining -= take;
        }
        Some(Self {
            sample_rate_hz,
            channel_count,
            total_frames,
            stereo_content,
            peak_amplitude,
            levels: build_levels(base),
        })
    }
}

fn read_u32(r: &mut impl Read) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Option<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_le_bytes(b))
}

fn stereo_content_to_byte(value: StereoContent) -> u8 {
    match value {
        StereoContent::Mono => 0,
        StereoContent::DualMono => 1,
        StereoContent::Stereo => 2,
        StereoContent::Multichannel => 3,
    }
}

fn stereo_content_from_byte(value: u8) -> Option<StereoContent> {
    Some(match value {
        0 => StereoContent::Mono,
        1 => StereoContent::DualMono,
        2 => StereoContent::Stereo,
        3 => StereoContent::Multichannel,
        _ => return None,
    })
}

fn build_levels(base: Vec<Bucket>) -> Vec<Vec<Bucket>> {
    let mut levels = vec![base];
    while levels.last().map_or(0, Vec::len) > MIN_LEVEL_LEN {
        let finer = levels.last().expect("levels is never empty");
        let coarser: Vec<Bucket> = finer
            .chunks(LEVEL_FACTOR)
            .map(|chunk| {
                chunk.iter().fold([i16::MAX, i16::MIN], |acc, b| {
                    [acc[0].min(b[0]), acc[1].max(b[1])]
                })
            })
            .collect();
        levels.push(coarser);
    }
    levels
}

fn to_i16_floor(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32).floor() as i16
}

fn to_i16_ceil(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32).ceil() as i16
}

/// Accumulates a [`Peaks`] pyramid from interleaved sample blocks.
pub struct PeakBuilder {
    sample_rate_hz: u32,
    channels: usize,
    base: Vec<Bucket>,
    bucket_min: f32,
    bucket_max: f32,
    bucket_frames: u32,
    total_frames: u64,
    peak_amplitude: f32,
    side_energy: f64,
    total_energy: f64,
}

impl PeakBuilder {
    pub fn new(sample_rate_hz: u32, channel_count: u16, expected_frames: Option<u64>) -> Self {
        let capacity = expected_frames
            .map(|frames| frames.div_ceil(BASE_FRAMES_PER_BUCKET as u64) as usize)
            .unwrap_or(0);
        Self {
            sample_rate_hz,
            channels: channel_count.max(1) as usize,
            base: Vec::with_capacity(capacity),
            bucket_min: f32::MAX,
            bucket_max: f32::MIN,
            bucket_frames: 0,
            total_frames: 0,
            peak_amplitude: 0.0,
            side_energy: 0.0,
            total_energy: 0.0,
        }
    }

    pub fn frames_seen(&self) -> u64 {
        self.total_frames
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(self.channels) {
            let mut lo = frame[0];
            let mut hi = frame[0];
            for &sample in &frame[1..] {
                lo = lo.min(sample);
                hi = hi.max(sample);
            }
            if self.channels == 2 {
                let side = (frame[0] - frame[1]) as f64;
                self.side_energy += side * side;
                self.total_energy +=
                    (frame[0] as f64) * (frame[0] as f64) + (frame[1] as f64) * (frame[1] as f64);
            }
            self.bucket_min = self.bucket_min.min(lo);
            self.bucket_max = self.bucket_max.max(hi);
            self.peak_amplitude = self.peak_amplitude.max(lo.abs()).max(hi.abs());
            self.bucket_frames += 1;
            if self.bucket_frames == BASE_FRAMES_PER_BUCKET {
                self.flush_bucket();
            }
        }
        self.total_frames += (interleaved.len() / self.channels) as u64;
    }

    fn flush_bucket(&mut self) {
        if self.bucket_frames == 0 {
            return;
        }
        self.base
            .push([to_i16_floor(self.bucket_min), to_i16_ceil(self.bucket_max)]);
        self.bucket_min = f32::MAX;
        self.bucket_max = f32::MIN;
        self.bucket_frames = 0;
    }

    pub fn finish(mut self) -> Peaks {
        self.flush_bucket();
        let stereo_content = match self.channels {
            1 => StereoContent::Mono,
            2 => {
                // Silence in both channels is trivially identical.
                if self.total_energy == 0.0
                    || self.side_energy / self.total_energy < DUAL_MONO_SIDE_ENERGY_RATIO
                {
                    StereoContent::DualMono
                } else {
                    StereoContent::Stereo
                }
            }
            _ => StereoContent::Multichannel,
        };
        Peaks {
            sample_rate_hz: self.sample_rate_hz,
            channel_count: self.channels as u16,
            total_frames: self.total_frames,
            stereo_content,
            peak_amplitude: self.peak_amplitude,
            levels: build_levels(self.base),
        }
    }
}

/// Decodes `path` from start to finish and summarises it. `on_progress`
/// receives a 0–1 fraction when the container declares its length, or `None`
/// when the length is unknown until the end. Setting `cancel` aborts the pass.
pub fn compute_peaks(
    path: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(Option<f32>),
) -> AppResult<Peaks> {
    let mut reader = AudioReader::open(path)?;
    reader.prime()?;
    let declared_frames = reader.declared_frame_count();
    let (Some(sample_rate_hz), Some(channel_count)) =
        (reader.sample_rate(), reader.channel_count())
    else {
        return Err(AppError::Decode {
            path: path.to_path_buf(),
            details: "the file contains no decodable audio".into(),
        });
    };

    let mut builder = PeakBuilder::new(sample_rate_hz, channel_count, declared_frames);
    let mut blocks = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(AppError::Cancelled);
        }
        let Some(block) = reader.next_block()? else {
            break;
        };
        builder.push(block);
        blocks += 1;
        if blocks % 64 == 0 {
            let fraction = declared_frames
                .filter(|total| *total > 0)
                .map(|total| (builder.frames_seen() as f64 / total as f64).min(1.0) as f32);
            on_progress(fraction);
        }
    }
    on_progress(Some(1.0));
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::super::decode::test_support::write_sine_wav;
    use super::*;

    fn sine(frames: usize, amplitude: f32, period: f32) -> Vec<f32> {
        (0..frames)
            .map(|n| amplitude * (n as f32 * std::f32::consts::TAU / period).sin())
            .collect()
    }

    #[test]
    fn builder_tracks_frames_and_amplitude() {
        let mut builder = PeakBuilder::new(48_000, 1, None);
        builder.push(&sine(10_000, 0.5, 100.0));
        let peaks = builder.finish();
        assert_eq!(peaks.total_frames(), 10_000);
        assert_eq!(peaks.levels[0].len(), 10_000usize.div_ceil(64));
        assert!((peaks.peak_amplitude - 0.5).abs() < 1e-3);
        let whole = peaks.query(0, 10_000, 1);
        assert!((whole[1] as f32 / 32767.0 - 0.5).abs() < 0.01);
        assert!((whole[0] as f32 / 32767.0 + 0.5).abs() < 0.01);
    }

    #[test]
    fn block_boundaries_do_not_change_the_result() {
        let samples = sine(5_000, 0.9, 37.0);
        let mut one = PeakBuilder::new(44_100, 1, None);
        one.push(&samples);
        let mut many = PeakBuilder::new(44_100, 1, None);
        for chunk in samples.chunks(113) {
            many.push(chunk);
        }
        assert_eq!(one.finish(), many.finish());
    }

    #[test]
    fn query_resolves_silence_then_signal() {
        let mut samples = vec![0.0f32; 8_000];
        samples.extend(sine(8_000, 0.8, 50.0));
        let mut builder = PeakBuilder::new(8_000, 1, None);
        builder.push(&samples);
        let peaks = builder.finish();

        let out = peaks.query(0, 16_000, 20);
        for i in 0..9 {
            assert_eq!(
                (out[i * 2], out[i * 2 + 1]),
                (0, 0),
                "bucket {i} should be silent"
            );
        }
        for i in 11..20 {
            assert!(out[i * 2 + 1] > 20_000, "bucket {i} should carry signal");
            assert!(out[i * 2] < -20_000);
        }
    }

    fn noisy(frames: usize) -> Peaks {
        let samples: Vec<f32> = (0..frames)
            .map(|n| ((n as f32 * 0.01).sin() * (n as f32 * 0.00007).cos()) * 0.9)
            .collect();
        let mut builder = PeakBuilder::new(44_100, 1, None);
        builder.push(&samples);
        builder.finish()
    }

    fn exact(peaks: &Peaks, from_frame: usize, to_frame: usize) -> (i16, i16) {
        let per = BASE_FRAMES_PER_BUCKET as usize;
        let slice = &peaks.levels[0][from_frame / per..to_frame.div_ceil(per)];
        (
            slice.iter().map(|b| b[0]).min().unwrap(),
            slice.iter().map(|b| b[1]).max().unwrap(),
        )
    }

    #[test]
    fn coarse_levels_agree_with_the_base_level() {
        // 25 columns of 16 384 frames: column edges fall on bucket edges at
        // every level, so the coarse answer must equal the exact one.
        let frames = 25 * 16_384;
        let peaks = noisy(frames);
        assert!(
            peaks.levels.len() >= 3,
            "expected a pyramid, got {} levels",
            peaks.levels.len()
        );

        let coarse = peaks.query(0, frames as u64, 25);
        for i in 0..25 {
            let expected = exact(&peaks, i * 16_384, (i + 1) * 16_384);
            assert_eq!((coarse[i * 2], coarse[i * 2 + 1]), expected, "column {i}");
        }
    }

    #[test]
    fn unaligned_columns_never_hide_a_peak() {
        // Column edges that do not line up with bucket edges may include a
        // sliver of the neighbouring column, but must never miss anything.
        let frames = 400_000;
        let peaks = noisy(frames);
        let columns = 37;
        let out = peaks.query(0, frames as u64, columns);
        let width = frames as f64 / columns as f64;
        for i in 0..columns {
            let from = (i as f64 * width).floor() as usize;
            let to = (((i + 1) as f64 * width).ceil() as usize).min(frames);
            let (min, max) = exact(&peaks, from, to);
            assert!(
                out[i * 2] <= min && out[i * 2 + 1] >= max,
                "column {i} lost a peak"
            );

            // ...and the bleed is bounded: widening the exact window by a
            // quarter column on each side always covers what was reported.
            let pad = (width / MIN_BUCKETS_PER_COLUMN as f64).ceil() as usize + 64;
            let (wide_min, wide_max) =
                exact(&peaks, from.saturating_sub(pad), (to + pad).min(frames));
            assert!(
                out[i * 2] >= wide_min && out[i * 2 + 1] <= wide_max,
                "column {i} bled too far"
            );
        }
    }

    #[test]
    fn query_handles_degenerate_ranges() {
        let mut builder = PeakBuilder::new(8_000, 1, None);
        builder.push(&sine(1_000, 0.5, 20.0));
        let peaks = builder.finish();
        assert!(peaks.query(0, 1_000, 0).is_empty());
        assert_eq!(peaks.query(500, 500, 4), vec![0; 8]);
        // Entirely past the end: silence.
        assert_eq!(peaks.query(5_000, 6_000, 4), vec![0; 8]);
        // Zoomed in beyond the base resolution still yields data.
        let zoomed = peaks.query(100, 110, 10);
        assert!(zoomed.chunks(2).all(|b| b[1] > b[0]));
    }

    #[test]
    fn classifies_stereo_content() {
        let left = sine(4_000, 0.5, 40.0);
        let right = sine(4_000, 0.5, 63.0);

        let mut identical = PeakBuilder::new(44_100, 2, None);
        identical.push(&left.iter().flat_map(|s| [*s, *s]).collect::<Vec<_>>());
        assert_eq!(identical.finish().stereo_content, StereoContent::DualMono);

        let mut different = PeakBuilder::new(44_100, 2, None);
        different.push(
            &left
                .iter()
                .zip(&right)
                .flat_map(|(l, r)| [*l, *r])
                .collect::<Vec<_>>(),
        );
        assert_eq!(different.finish().stereo_content, StereoContent::Stereo);

        let mut mono = PeakBuilder::new(44_100, 1, None);
        mono.push(&left);
        assert_eq!(mono.finish().stereo_content, StereoContent::Mono);
    }

    #[test]
    fn cache_round_trips() {
        let mut builder = PeakBuilder::new(44_100, 2, None);
        builder.push(&sine(90_001 * 2, 0.7, 91.0));
        let peaks = builder.finish();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("peaks.bin");
        peaks.save(&path).unwrap();
        assert_eq!(Peaks::load(&path), Some(peaks));
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peaks.bin");
        assert_eq!(Peaks::load(&path), None);
        std::fs::write(&path, b"VSPK\x01\x00\x00\x00truncated").unwrap();
        assert_eq!(Peaks::load(&path), None);
        std::fs::write(&path, b"nonsense").unwrap();
        assert_eq!(Peaks::load(&path), None);
    }

    #[test]
    fn computes_peaks_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        write_sine_wav(&path, 44_100, 2.0, 0.6, &[330.0, 550.0]);

        let mut reports = 0;
        let cancel = AtomicBool::new(false);
        let peaks = compute_peaks(&path, &cancel, |fraction| {
            reports += 1;
            let fraction = fraction.expect("WAV declares its length");
            assert!((0.0..=1.0).contains(&fraction));
        })
        .unwrap();

        assert_eq!(peaks.total_frames(), 88_200);
        assert_eq!(peaks.sample_rate_hz(), 44_100);
        let summary = peaks.summary();
        assert_eq!(summary.channel_count, 2);
        assert_eq!(summary.stereo_content, StereoContent::Stereo);
        assert!((summary.duration_seconds - 2.0).abs() < 1e-9);
        assert!((summary.peak_dbfs.unwrap() - 20.0 * 0.6f32.log10()).abs() < 0.1);
        assert!(reports > 0);
    }

    #[test]
    fn computing_peaks_can_be_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_sine_wav(&path, 44_100, 1.0, 0.5, &[440.0]);
        let cancel = AtomicBool::new(true);
        let err = compute_peaks(&path, &cancel, |_| {}).unwrap_err();
        assert!(matches!(err, AppError::Cancelled));
    }
}
