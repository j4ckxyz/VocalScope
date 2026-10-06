//! Vocal isolation: taking a full mix and producing its vocals alone.
//!
//! The work is done by an MDX-Net model run in-process through ONNX Runtime.
//! This module decodes the recording, brings it to the model's format
//! (stereo, 44.1 kHz), streams it through the model and writes the vocals to
//! a WAV file. Like everything else in the core, it never holds a whole
//! recording in memory.

pub mod mdx;
pub mod models;
pub mod resample;
pub mod stft;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::audio::decode::AudioReader;
use crate::error::{AppError, AppResult};
use mdx::{MdxParameters, MdxSeparator, OnnxModel, SpectrogramModel, MODEL_SAMPLE_RATE_HZ};
use resample::Resampler;

/// Rearranges interleaved audio with any channel count into interleaved
/// stereo: mono is doubled, and of more than two channels the first two are
/// kept.
fn to_stereo(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    match channels {
        1 => out.extend(interleaved.iter().flat_map(|sample| [*sample, *sample])),
        2 => out.extend_from_slice(interleaved),
        _ => out.extend(
            interleaved
                .chunks_exact(channels)
                .flat_map(|frame| [frame[0], frame[1]]),
        ),
    }
}

fn write_error(err: hound::Error) -> AppError {
    match err {
        hound::Error::IoError(err) => AppError::Io(err),
        other => AppError::Separation(format!("could not write the vocals: {other}")),
    }
}

/// Isolates the vocals of `source` with the model at `model_path` and writes
/// them to `output` as a 16-bit stereo WAV. `on_progress` receives 0–1, or
/// `None` when the recording's length is not known in advance.
///
/// The output appears only when the whole file has been written; a cancelled
/// or failed run leaves nothing behind.
pub fn isolate_vocals(
    source: &Path,
    model_path: &Path,
    parameters: MdxParameters,
    threads: usize,
    output: &Path,
    cancel: &AtomicBool,
    on_progress: impl FnMut(Option<f32>),
) -> AppResult<()> {
    let model = OnnxModel::load(model_path, threads)?;
    isolate_vocals_with(
        source,
        Box::new(model),
        parameters,
        output,
        cancel,
        on_progress,
    )
}

/// [`isolate_vocals`] with the model supplied by the caller.
pub fn isolate_vocals_with(
    source: &Path,
    model: Box<dyn SpectrogramModel>,
    parameters: MdxParameters,
    output: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(Option<f32>),
) -> AppResult<()> {
    let mut reader = AudioReader::open(source)?;
    reader.prime()?;
    let rate = reader.sample_rate().ok_or_else(|| AppError::Decode {
        path: source.to_path_buf(),
        details: "sample rate could not be determined".into(),
    })?;
    let channels = reader.channel_count().unwrap_or(1).max(1) as usize;
    let expected_frames = reader
        .declared_frame_count()
        .map(|frames| frames as f64 * MODEL_SAMPLE_RATE_HZ as f64 / rate as f64)
        .filter(|frames| *frames > 0.0);

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = output.with_extension("part");
    let result = (|| -> AppResult<()> {
        let mut writer = hound::WavWriter::create(
            &partial,
            hound::WavSpec {
                channels: 2,
                sample_rate: MODEL_SAMPLE_RATE_HZ,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .map_err(write_error)?;

        let mut separator = MdxSeparator::new(model, parameters);
        let mut resampler = Resampler::new(rate, MODEL_SAMPLE_RATE_HZ, 2);
        let should_stop = || cancel.load(Ordering::Relaxed);
        let (mut stereo, mut converted, mut vocals) = (Vec::new(), Vec::new(), Vec::new());
        let mut write = |vocals: &mut Vec<f32>, frames_out: u64| -> AppResult<()> {
            for sample in vocals.drain(..) {
                let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                writer.write_sample(scaled).map_err(write_error)?;
            }
            on_progress(expected_frames.map(|total| (frames_out as f64 / total).min(1.0) as f32));
            Ok(())
        };

        while let Some(block) = reader.next_block()? {
            if should_stop() {
                return Err(AppError::Cancelled);
            }
            to_stereo(block, channels, &mut stereo);
            let input = match &mut resampler {
                Some(resampler) => {
                    converted.clear();
                    resampler.push(&stereo, &mut converted);
                    &converted
                }
                None => &stereo,
            };
            separator.push(input, &mut vocals, &should_stop)?;
            if !vocals.is_empty() {
                write(&mut vocals, separator.frames_out())?;
            }
        }
        if let Some(resampler) = resampler {
            converted.clear();
            resampler.finish(&mut converted);
            separator.push(&converted, &mut vocals, &should_stop)?;
        }
        separator.finish(&mut vocals, &should_stop)?;
        write(&mut vocals, u64::MAX)?;
        writer.finalize().map_err(write_error)
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&partial, output)?;
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_file(&partial);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use super::mdx::test_support::{ScalingModel, SMALL};
    use super::*;
    use crate::analysis::pitch::test_support::synth;
    use crate::analysis::test_support::{write_wav, VOICE};
    use crate::audio::decode::probe;

    fn model(gain: f32) -> Box<ScalingModel> {
        Box::new(ScalingModel {
            gain,
            runs: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn read(path: &Path) -> Vec<f32> {
        let mut reader = AudioReader::open(path).unwrap();
        let mut samples = Vec::new();
        while let Some(block) = reader.next_block().unwrap() {
            samples.extend_from_slice(block);
        }
        samples
    }

    #[test]
    fn a_file_goes_through_the_model_and_comes_out_as_a_stem() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("mix.wav");
        let tone = synth(44_100, 2.0, &VOICE, |_| 60.0, |_| 0.4);
        write_wav(&source, 44_100, &[&tone, &tone]);
        let output = dir.path().join("stems").join("vocals.wav");

        let mut reports = Vec::new();
        isolate_vocals_with(
            &source,
            model(0.5),
            SMALL,
            &output,
            &AtomicBool::new(false),
            |f| reports.push(f),
        )
        .unwrap();

        let info = probe(&output).unwrap();
        assert_eq!((info.sample_rate_hz, info.channel_count), (44_100, 2));
        assert_eq!(info.frame_count, Some(88_200));
        assert_eq!(*reports.last().unwrap(), Some(1.0));
        let samples = read(&output);
        for (i, frame) in samples.chunks(2).enumerate().skip(100).step_by(37) {
            assert!((frame[0] - 0.5 * tone[i]).abs() < 1e-3, "frame {i}");
            assert_eq!(frame[1], 0.0);
        }
        assert!(!output.with_extension("part").exists());
    }

    #[test]
    fn mono_and_other_sample_rates_are_brought_to_the_models_format() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("mono48.wav");
        let tone = synth(48_000, 1.0, &[1.0], |_| 69.0, |_| 0.5);
        write_wav(&source, 48_000, &[&tone]);
        let output = dir.path().join("vocals.wav");
        isolate_vocals_with(
            &source,
            model(1.0),
            SMALL,
            &output,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        let info = probe(&output).unwrap();
        assert_eq!((info.sample_rate_hz, info.channel_count), (44_100, 2));
        assert_eq!(info.frame_count, Some(44_100));
        // Still an A at 440 Hz, the same length, at the same level.
        let expected = synth(44_100, 1.0, &[1.0], |_| 69.0, |_| 0.5);
        let samples = read(&output);
        let worst = (500..43_500)
            .step_by(11)
            .map(|i| (samples[i * 2] - expected[i]).abs())
            .fold(0f32, f32::max);
        assert!(worst < 5e-3, "worst error {worst}");
    }

    #[test]
    fn cancelling_or_failing_leaves_no_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("mix.wav");
        let tone = synth(44_100, 8.0, &VOICE, |_| 60.0, |_| 0.4);
        write_wav(&source, 44_100, &[&tone, &tone]);
        let output = dir.path().join("vocals.wav");

        let cancelled = isolate_vocals_with(
            &source,
            model(1.0),
            SMALL,
            &output,
            &AtomicBool::new(true),
            |_| {},
        );
        assert!(matches!(cancelled.unwrap_err(), AppError::Cancelled));

        let missing_model = isolate_vocals(
            &source,
            &dir.path().join("no-model.onnx"),
            SMALL,
            2,
            &output,
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(matches!(
            missing_model.unwrap_err(),
            AppError::Separation(_)
        ));

        let missing_source = isolate_vocals_with(
            &dir.path().join("absent.wav"),
            model(1.0),
            SMALL,
            &output,
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(matches!(
            missing_source.unwrap_err(),
            AppError::FileNotFound(_)
        ));

        assert!(!output.exists());
        assert!(!output.with_extension("part").exists());
    }

    #[test]
    fn channel_layouts_are_reduced_to_stereo() {
        let mut out = Vec::new();
        to_stereo(&[0.1, 0.2], 1, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2]);
        to_stereo(&[0.1, 0.2, 0.3, 0.4], 2, &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        to_stereo(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 3, &mut out);
        assert_eq!(out, [0.1, 0.2, 0.4, 0.5]);
    }
}
