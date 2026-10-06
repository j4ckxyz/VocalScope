//! Audio import, decoding, waveform summarisation and playback.
//!
//! Decoding is streamed packet-by-packet through Symphonia; nothing in this
//! module loads a whole recording into memory.

pub mod decode;
pub mod peaks;
pub mod playback;

use serde::{Deserialize, Serialize};

/// Technical description of an audio file, read without modifying it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
pub struct AudioInfo {
    pub file_name: String,
    pub file_extension: Option<String>,
    pub file_size_bytes: u64,
    /// Short codec identifier, e.g. `mp3`, `flac`, `pcm_s16le`.
    pub codec: String,
    pub codec_description: String,
    pub sample_rate_hz: u32,
    pub channel_count: u16,
    /// Bits per sample for lossless/PCM audio; `None` for lossy codecs where
    /// the concept does not apply.
    pub bit_depth: Option<u32>,
    /// Length in sample frames, when the container declares it. The waveform
    /// pass replaces this with an exact count.
    pub frame_count: Option<u64>,
    pub duration_seconds: Option<f64>,
    /// File size divided by duration; includes container overhead.
    pub average_bitrate_kbps: Option<u32>,
    pub tags: Tags,
}

/// Embedded metadata exactly as found in the file. Absent fields stay `None`;
/// nothing here is inferred or guessed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, uniffi::Record)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<String>,
    pub genre: Option<String>,
}

/// Whether a file's channels actually carry different signals.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum StereoContent {
    /// A single channel.
    Mono,
    /// Two channels carrying the same signal.
    DualMono,
    /// Two channels with distinct content.
    Stereo,
    /// More than two channels.
    Multichannel,
}
