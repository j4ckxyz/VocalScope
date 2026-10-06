//! Streaming audio decoding via Symphonia.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTagKey};
use symphonia::core::probe::Hint;

use super::{AudioInfo, Tags};
use crate::error::{AppError, AppResult};

/// Give up on a file once this many packets have failed to decode; a handful
/// of bad packets in an otherwise healthy file are skipped silently.
const MAX_SKIPPED_PACKETS: u32 = 2_000;

/// Sequential reader that yields interleaved `f32` sample blocks.
pub struct AudioReader {
    path: PathBuf,
    file_size_bytes: u64,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    params: CodecParameters,
    tags: Tags,
    spec: Option<SignalSpec>,
    sample_buf: Option<SampleBuffer<f32>>,
    buf_frame_capacity: usize,
    /// A block decoded by [`AudioReader::prime`] that has not been handed out yet.
    primed: bool,
    skipped_packets: u32,
}

impl AudioReader {
    pub fn open(path: &Path) -> AppResult<Self> {
        let file = File::open(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => AppError::FileNotFound(path.to_path_buf()),
            _ => AppError::Io(err),
        })?;
        let file_size_bytes = file.metadata()?.len();
        let stream = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        // Gapless trimming removes encoder delay/padding so that sample
        // positions here match the playback engine, which also enables it.
        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };

        let unsupported = |details: String| AppError::UnsupportedFormat {
            path: path.to_path_buf(),
            details,
        };

        let mut probed = symphonia::default::get_probe()
            .format(&hint, stream, &format_opts, &MetadataOptions::default())
            .map_err(|err| unsupported(err.to_string()))?;

        // Tags can live inside the container or in a wrapper read before it
        // (e.g. an ID3v2 header in front of an MP3 stream). Container tags win.
        let mut tags = Tags::default();
        if let Some(revision) = probed.format.metadata().skip_to_latest() {
            merge_tags(&mut tags, revision);
        }
        if let Some(mut metadata) = probed.metadata.get() {
            if let Some(revision) = metadata.skip_to_latest() {
                merge_tags(&mut tags, revision);
            }
        }

        let track = probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| unsupported("the file contains no audio track".into()))?;
        let track_id = track.id;
        let params = track.codec_params.clone();

        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|err| unsupported(format!("no decoder available: {err}")))?;

        Ok(Self {
            path: path.to_path_buf(),
            file_size_bytes,
            format: probed.format,
            decoder,
            track_id,
            params,
            tags,
            spec: None,
            sample_buf: None,
            buf_frame_capacity: 0,
            primed: false,
            skipped_packets: 0,
        })
    }

    /// Sample rate, known up front for most formats and always after the
    /// first decoded block.
    pub fn sample_rate(&self) -> Option<u32> {
        self.spec.map(|s| s.rate).or(self.params.sample_rate)
    }

    pub fn channel_count(&self) -> Option<u16> {
        self.spec
            .map(|s| s.channels.count() as u16)
            .or_else(|| self.params.channels.map(|c| c.count() as u16))
    }

    /// Frame count declared by the container, if any.
    pub fn declared_frame_count(&self) -> Option<u64> {
        self.params.n_frames
    }

    /// Decodes the first block (if not already done) so that the sample rate
    /// and channel count are known for formats that do not declare them in
    /// their header. The block is kept and returned by the next
    /// [`AudioReader::next_block`] call, so no audio is skipped.
    pub fn prime(&mut self) -> AppResult<()> {
        if self.spec.is_none() && self.next_block()?.is_some() {
            self.primed = true;
        }
        Ok(())
    }

    /// Decodes the next packet and returns its interleaved samples, or `None`
    /// at the end of the stream.
    pub fn next_block(&mut self) -> AppResult<Option<&[f32]>> {
        if self.primed {
            self.primed = false;
            return Ok(self.sample_buf.as_ref().map(|buf| buf.samples()));
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                // Symphonia signals a normal end of stream as an unexpected EOF.
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => return Ok(None),
                Err(err) => return Err(self.decode_error(err.to_string())),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.frames() == 0 {
                        continue;
                    }
                    let spec = *decoded.spec();
                    if self.spec != Some(spec) || decoded.capacity() > self.buf_frame_capacity {
                        self.buf_frame_capacity = decoded.capacity();
                        self.sample_buf =
                            Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                        self.spec = Some(spec);
                    }
                    if let Some(buf) = self.sample_buf.as_mut() {
                        buf.copy_interleaved_ref(decoded);
                    }
                    break;
                }
                Err(SymphoniaError::DecodeError(reason)) => {
                    self.skipped_packets += 1;
                    if self.skipped_packets > MAX_SKIPPED_PACKETS {
                        return Err(self.decode_error(format!(
                            "too many undecodable packets (last error: {reason})"
                        )));
                    }
                }
                Err(err) => return Err(self.decode_error(err.to_string())),
            }
        }
        Ok(self.sample_buf.as_ref().map(|buf| buf.samples()))
    }

    /// Number of corrupt packets skipped so far.
    pub fn skipped_packets(&self) -> u32 {
        self.skipped_packets
    }

    fn decode_error(&self, details: String) -> AppError {
        AppError::Decode {
            path: self.path.clone(),
            details,
        }
    }

    /// Builds the technical description of the file. Decodes one block first
    /// when the container does not declare the sample rate or channel layout.
    pub fn info(&mut self) -> AppResult<AudioInfo> {
        if self.sample_rate().is_none() || self.channel_count().is_none() {
            self.prime()?;
        }
        let sample_rate_hz = self
            .sample_rate()
            .ok_or_else(|| self.decode_error("sample rate could not be determined".into()))?;
        let channel_count = self
            .channel_count()
            .ok_or_else(|| self.decode_error("channel count could not be determined".into()))?;

        let (codec, codec_description) =
            match symphonia::default::get_codecs().get_codec(self.params.codec) {
                Some(descriptor) => (
                    descriptor.short_name.to_string(),
                    descriptor.long_name.to_string(),
                ),
                None => ("unknown".to_string(), "Unknown codec".to_string()),
            };

        let frame_count = self.params.n_frames;
        let duration_seconds = frame_count.map(|frames| frames as f64 / sample_rate_hz as f64);
        let average_bitrate_kbps = duration_seconds
            .filter(|d| *d > 0.0)
            .map(|d| ((self.file_size_bytes as f64 * 8.0) / d / 1000.0).round() as u32);

        Ok(AudioInfo {
            file_name: self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            file_extension: self
                .path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase()),
            file_size_bytes: self.file_size_bytes,
            codec,
            codec_description,
            sample_rate_hz,
            channel_count,
            bit_depth: self.params.bits_per_sample,
            frame_count,
            duration_seconds,
            average_bitrate_kbps,
            tags: self.tags.clone(),
        })
    }
}

/// Reads a file's technical metadata and tags without decoding its audio.
pub fn probe(path: &Path) -> AppResult<AudioInfo> {
    AudioReader::open(path)?.info()
}

fn merge_tags(tags: &mut Tags, revision: &MetadataRevision) {
    fn set(slot: &mut Option<String>, value: String) {
        let value = value.trim().to_string();
        if slot.is_none() && !value.is_empty() {
            *slot = Some(value);
        }
    }
    for tag in revision.tags() {
        let Some(key) = tag.std_key else { continue };
        let value = tag.value.to_string();
        match key {
            StandardTagKey::TrackTitle => set(&mut tags.title, value),
            StandardTagKey::Artist => set(&mut tags.artist, value),
            StandardTagKey::Album => set(&mut tags.album, value),
            StandardTagKey::AlbumArtist => set(&mut tags.album_artist, value),
            StandardTagKey::Genre => set(&mut tags.genre, value),
            StandardTagKey::TrackNumber => set(&mut tags.track_number, value),
            StandardTagKey::Date | StandardTagKey::ReleaseDate | StandardTagKey::OriginalDate => {
                if tags.year.is_none() {
                    tags.year = parse_year(&value);
                }
            }
            _ => {}
        }
    }
}

/// Extracts a four-digit year from a date tag such as `1976`, `1976-11-12`
/// or `12/11/1976`.
pub fn parse_year(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 4 {
                let year: i32 = value[start..i].parse().ok()?;
                if (1000..=2999).contains(&year) {
                    return Some(year);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::f32::consts::TAU;
    use std::path::Path;

    /// Writes a 16-bit PCM WAV whose channels are sine waves at the given
    /// frequencies (one frequency per channel).
    pub fn write_sine_wav(
        path: &Path,
        sample_rate: u32,
        seconds: f32,
        amplitude: f32,
        channel_freqs: &[f32],
    ) {
        let spec = hound::WavSpec {
            channels: channel_freqs.len() as u16,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let frames = (sample_rate as f32 * seconds) as u32;
        for n in 0..frames {
            let t = n as f32 / sample_rate as f32;
            for freq in channel_freqs {
                let sample = amplitude * (TAU * freq * t).sin();
                writer
                    .write_sample((sample * i16::MAX as f32) as i16)
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::write_sine_wav;
    use super::*;

    #[test]
    fn parses_years_from_common_date_formats() {
        assert_eq!(parse_year("1976"), Some(1976));
        assert_eq!(parse_year("1976-11-12"), Some(1976));
        assert_eq!(parse_year("12/11/1976"), Some(1976));
        assert_eq!(parse_year("2011-09-05T00:00:00Z"), Some(2011));
        assert_eq!(parse_year("unknown"), None);
        assert_eq!(parse_year("76"), None);
        assert_eq!(parse_year("19761112"), None);
    }

    #[test]
    fn probes_wav_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_sine_wav(&path, 44_100, 1.5, 0.5, &[440.0, 440.0]);

        let info = probe(&path).unwrap();
        assert_eq!(info.file_name, "tone.wav");
        assert_eq!(info.file_extension.as_deref(), Some("wav"));
        assert_eq!(info.sample_rate_hz, 44_100);
        assert_eq!(info.channel_count, 2);
        assert_eq!(info.bit_depth, Some(16));
        assert_eq!(info.frame_count, Some(66_150));
        assert!((info.duration_seconds.unwrap() - 1.5).abs() < 1e-6);
        assert!(info.codec.starts_with("pcm"));
        assert_eq!(info.tags, Tags::default());
    }

    #[test]
    fn decodes_every_frame_of_a_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");
        write_sine_wav(&path, 22_050, 2.0, 0.8, &[220.0]);

        let mut reader = AudioReader::open(&path).unwrap();
        let mut samples = 0usize;
        let mut peak = 0f32;
        while let Some(block) = reader.next_block().unwrap() {
            samples += block.len();
            peak = block.iter().fold(peak, |acc, s| acc.max(s.abs()));
        }
        assert_eq!(samples, 44_100);
        assert!((peak - 0.8).abs() < 0.01, "peak was {peak}");
        assert_eq!(reader.skipped_packets(), 0);
    }

    #[test]
    fn missing_file_is_reported_as_not_found() {
        let err = probe(Path::new("/definitely/not/here.wav")).unwrap_err();
        assert!(matches!(err, AppError::FileNotFound(_)));
    }

    #[test]
    fn non_audio_file_is_reported_as_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.mp3");
        std::fs::write(
            &path,
            b"this is plainly not an mp3 file at all, just text. ".repeat(200),
        )
        .unwrap();
        let err = probe(&path).unwrap_err();
        assert!(
            matches!(err, AppError::UnsupportedFormat { .. }),
            "got {err:?}"
        );
    }
}
