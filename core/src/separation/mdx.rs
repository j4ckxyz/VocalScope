//! Running an MDX-Net separation model over a stream of audio.
//!
//! The model maps the spectrogram of about six seconds of stereo mixture to
//! the spectrogram of the vocals in it. Audio is fed through in overlapping
//! chunks; the ends of each chunk, where the model has least context, are
//! discarded and only the middles are joined up.

use std::path::Path;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use super::stft::Stft;
use crate::error::{AppError, AppResult};

/// The models' sample rate, hop size and frames per chunk.
pub const MODEL_SAMPLE_RATE_HZ: u32 = 44_100;
pub const HOP: usize = 1_024;
pub const DIM_T: usize = 256;
/// Real and imaginary parts of left and right.
const TENSOR_CHANNELS: usize = 4;

/// The numbers that fix how one model's spectrograms are computed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MdxParameters {
    pub n_fft: usize,
    pub dim_f: usize,
    /// The model's output is this much too quiet; published with the model.
    pub compensation: f32,
}

/// Maps a mixture spectrogram to a vocal spectrogram. Both are
/// `4 × dim_f × dim_t` values: left real, left imaginary, right real, right
/// imaginary.
pub trait SpectrogramModel: Send {
    fn run(&mut self, spectrogram: Vec<f32>, dim_f: usize, dim_t: usize) -> AppResult<Vec<f32>>;
}

/// An MDX-Net model loaded into ONNX Runtime.
pub struct OnnxModel {
    session: Session,
    input_name: String,
}

fn inference_error(err: impl std::fmt::Display) -> AppError {
    AppError::Separation(err.to_string())
}

impl OnnxModel {
    /// Loads a model file. `threads` is the number of CPU threads inference
    /// may use.
    pub fn load(path: &Path, threads: usize) -> AppResult<Self> {
        let mut builder = Session::builder()
            .map_err(inference_error)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(inference_error)?
            .with_intra_threads(threads.max(1))
            .map_err(inference_error)?;
        let session = builder.commit_from_file(path).map_err(inference_error)?;
        let input_name = session
            .inputs()
            .first()
            .map(|input| input.name().to_string())
            .ok_or_else(|| AppError::Separation("the model has no input".into()))?;
        Ok(Self {
            session,
            input_name,
        })
    }
}

impl SpectrogramModel for OnnxModel {
    fn run(&mut self, spectrogram: Vec<f32>, dim_f: usize, dim_t: usize) -> AppResult<Vec<f32>> {
        let expected = spectrogram.len();
        let input = Tensor::from_array(([1usize, TENSOR_CHANNELS, dim_f, dim_t], spectrogram))
            .map_err(inference_error)?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input])
            .map_err(inference_error)?;
        let (_, values) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(inference_error)?;
        if values.len() != expected {
            return Err(AppError::Separation(format!(
                "the model returned {} values where {expected} were expected",
                values.len()
            )));
        }
        Ok(values.to_vec())
    }
}

/// Feeds stereo audio through a model chunk by chunk.
pub struct MdxSeparator {
    model: Box<dyn SpectrogramModel>,
    stft: Stft,
    compensation: f32,
    /// Samples at each end of a chunk that are discarded.
    trim: usize,
    /// Buffered input per channel, beginning with `trim` samples of silence.
    input: [Vec<f32>; 2],
    frames_in: u64,
    frames_out: u64,
    chunk: Vec<f32>,
}

impl MdxSeparator {
    pub fn new(model: Box<dyn SpectrogramModel>, parameters: MdxParameters) -> Self {
        let stft = Stft::new(parameters.n_fft, HOP, parameters.dim_f, DIM_T);
        let trim = parameters.n_fft / 2;
        Self {
            chunk: vec![0.0; stft.chunk_len()],
            stft,
            model,
            compensation: parameters.compensation,
            trim,
            input: [vec![0.0; trim], vec![0.0; trim]],
            frames_in: 0,
            frames_out: 0,
        }
    }

    /// New samples consumed, and produced, per model run.
    pub fn step_len(&self) -> usize {
        self.stft.chunk_len() - 2 * self.trim
    }

    /// Frames of vocals produced so far.
    pub fn frames_out(&self) -> u64 {
        self.frames_out
    }

    /// Feeds interleaved stereo at 44.1 kHz; appends interleaved stereo
    /// vocals to `out` as chunks complete. `should_stop` is checked before
    /// each model run, which is the slow part.
    pub fn push(
        &mut self,
        interleaved: &[f32],
        out: &mut Vec<f32>,
        should_stop: &dyn Fn() -> bool,
    ) -> AppResult<()> {
        for frame in interleaved.chunks_exact(2) {
            self.input[0].push(frame[0]);
            self.input[1].push(frame[1]);
        }
        self.frames_in += (interleaved.len() / 2) as u64;
        while self.input[0].len() >= self.stft.chunk_len() {
            if should_stop() {
                return Err(AppError::Cancelled);
            }
            self.run_chunk(out, self.step_len())?;
        }
        Ok(())
    }

    /// Processes what is left. The total output has exactly as many frames
    /// as were pushed.
    pub fn finish(mut self, out: &mut Vec<f32>, should_stop: &dyn Fn() -> bool) -> AppResult<()> {
        while self.frames_out < self.frames_in {
            if should_stop() {
                return Err(AppError::Cancelled);
            }
            let missing = self.stft.chunk_len().saturating_sub(self.input[0].len());
            for channel in &mut self.input {
                channel.extend(std::iter::repeat_n(0.0, missing));
            }
            let wanted = (self.frames_in - self.frames_out).min(self.step_len() as u64) as usize;
            self.run_chunk(out, wanted)?;
        }
        Ok(())
    }

    /// Runs the model on the first chunk in the buffer and emits `keep`
    /// frames from its middle.
    fn run_chunk(&mut self, out: &mut Vec<f32>, keep: usize) -> AppResult<()> {
        let (dim_f, dim_t) = (self.stft.dim_f, self.stft.dim_t);
        let plane = dim_f * dim_t;
        let chunk_len = self.stft.chunk_len();

        let mut spectrogram = vec![0f32; TENSOR_CHANNELS * plane];
        for (channel, samples) in self.input.iter().enumerate() {
            let (real, imaginary) =
                spectrogram[channel * 2 * plane..(channel * 2 + 2) * plane].split_at_mut(plane);
            self.stft.forward(&samples[..chunk_len], real, imaginary);
        }
        let vocals = self.model.run(spectrogram, dim_f, dim_t)?;

        let start = out.len();
        out.resize(start + keep * 2, 0.0);
        for channel in 0..2 {
            let real = &vocals[channel * 2 * plane..(channel * 2 + 1) * plane];
            let imaginary = &vocals[(channel * 2 + 1) * plane..(channel * 2 + 2) * plane];
            self.stft.inverse(real, imaginary, &mut self.chunk);
            for (i, sample) in self.chunk[self.trim..self.trim + keep].iter().enumerate() {
                out[start + i * 2 + channel] = sample * self.compensation;
            }
        }
        self.frames_out += keep as u64;
        let step = self.step_len();
        for channel in &mut self.input {
            channel.drain(..step);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Stands in for a real model: returns the mixture scaled by `gain`,
    /// with the right channel silenced, and counts its runs.
    pub struct ScalingModel {
        pub gain: f32,
        pub runs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SpectrogramModel for ScalingModel {
        fn run(&mut self, mut s: Vec<f32>, dim_f: usize, dim_t: usize) -> AppResult<Vec<f32>> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(s.len(), 4 * dim_f * dim_t);
            let plane = dim_f * dim_t;
            for value in &mut s[..2 * plane] {
                *value *= self.gain;
            }
            s[2 * plane..].fill(0.0);
            Ok(s)
        }
    }

    /// Small enough to run in milliseconds; keeps every frequency bin.
    pub const SMALL: MdxParameters = MdxParameters {
        n_fft: 2_048,
        dim_f: 1_025,
        compensation: 1.0,
    };
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::test_support::*;
    use super::*;
    use crate::analysis::pitch::test_support::noise;

    fn separator(gain: f32, compensation: f32) -> (MdxSeparator, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let model = ScalingModel {
            gain,
            runs: runs.clone(),
        };
        let parameters = MdxParameters {
            compensation,
            ..SMALL
        };
        (MdxSeparator::new(Box::new(model), parameters), runs)
    }

    #[test]
    fn chunks_are_joined_without_seams_or_shift() {
        let (mut separator, runs) = separator(0.5, 1.0);
        let step = separator.step_len();
        assert_eq!(step, HOP * (DIM_T - 1) - SMALL.n_fft);
        // Two and a bit chunks, fed in awkward pieces.
        let frames = step * 2 + 12_345;
        let left = noise(frames, 21);
        let right = noise(frames, 22);
        let interleaved: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(l, r)| [*l, *r])
            .collect();

        let mut out = Vec::new();
        for block in interleaved.chunks(2 * 70_001) {
            separator.push(block, &mut out, &|| false).unwrap();
        }
        assert_eq!(separator.frames_out(), 2 * step as u64);
        separator.finish(&mut out, &|| false).unwrap();

        assert_eq!(out.len(), frames * 2);
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let mut worst = 0f32;
        for (i, frame) in out.chunks(2).enumerate() {
            worst = worst.max((frame[0] - 0.5 * left[i]).abs());
            assert_eq!(frame[1], 0.0);
        }
        // The same sample-for-sample, including across both chunk joins.
        assert!(worst < 2e-4, "worst error {worst}");
    }

    #[test]
    fn the_compensation_factor_is_applied() {
        let (mut separator, _) = separator(1.0, 1.25);
        let frames = 5_000;
        let left = noise(frames, 3);
        let interleaved: Vec<f32> = left.iter().flat_map(|l| [*l, 0.0]).collect();
        let mut out = Vec::new();
        separator.push(&interleaved, &mut out, &|| false).unwrap();
        assert!(out.is_empty(), "nothing is emitted until a chunk is full");
        separator.finish(&mut out, &|| false).unwrap();
        assert_eq!(out.len(), frames * 2);
        for (i, frame) in out.chunks(2).enumerate().skip(10) {
            assert!((frame[0] - 1.25 * left[i]).abs() < 3e-4);
        }
    }

    #[test]
    fn stopping_is_honoured_before_each_model_run() {
        let (mut separator, runs) = separator(1.0, 1.0);
        let silence = vec![0f32; 2 * (HOP * DIM_T * 2)];
        let mut out = Vec::new();
        let result = separator.push(&silence, &mut out, &|| true);
        assert!(matches!(result.unwrap_err(), AppError::Cancelled));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn empty_input_yields_empty_output_without_running_the_model() {
        let (separator, runs) = separator(1.0, 1.0);
        let mut out = Vec::new();
        separator.finish(&mut out, &|| false).unwrap();
        assert!(out.is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_missing_model_file_is_a_separation_error() {
        let result = OnnxModel::load(Path::new("/no/such/model.onnx"), 1);
        assert!(matches!(result.err().unwrap(), AppError::Separation(_)));
    }
}
