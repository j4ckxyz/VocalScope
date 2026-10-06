//! Exporting an analysis for use outside VocalScope.
//!
//! * JSON — everything, for other software.
//! * CSV — the pitch curve frame by frame, or the notes, for spreadsheets.
//! * MIDI — the notes, for a sequencer or notation program.
//! * Report — a plain-language summary in Markdown, readable as it is.
//!
//! Every format that carries the correction indicators carries their caveat
//! with them, so an exported result cannot be quoted without it.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use serde_json::json;

use crate::analysis::compare::{Alignment, AlignmentQuality, PitchComparison};
use crate::analysis::indicators::CAVEAT;
use crate::analysis::notes::describe_pitch;
use crate::analysis::pitch::hz_to_midi;
use crate::analysis::Analysis;
use crate::error::{AppError, AppResult};
use crate::ffi_types::Timestamp;
use crate::project::{Recording, SourceKind};
use crate::timeline::format_time;

/// Version of the JSON document's shape.
pub const EXPORT_FORMAT_VERSION: u32 = 1;
/// MIDI timing: 480 ticks per quarter note at 120 beats per minute, which
/// makes one second exactly 960 ticks.
const MIDI_TICKS_PER_QUARTER: u16 = 480;
const MIDI_MICROSECONDS_PER_QUARTER: u32 = 500_000;
const MIDI_TICKS_PER_SECOND: f64 = 960.0;
/// The report lists at most this many notes and differing passages; the CSV
/// and JSON exports have them all.
const REPORT_MAX_NOTES: usize = 300;
const REPORT_MAX_REGIONS: usize = 40;

pub const FULL_MIX_CAUTION: &str = "This analysis was made from the recording as it is. If it contains instruments or more than one voice, they affect the pitch curve and every figure derived from it; isolating the vocals first gives a more dependable result.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExportFormat {
    Json,
    PitchCsv,
    NotesCsv,
    Midi,
    Report,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::PitchCsv | ExportFormat::NotesCsv => "csv",
            ExportFormat::Midi => "mid",
            ExportFormat::Report => "md",
        }
    }

    /// Added to the recording's name to suggest a file name.
    pub fn name_suffix(self) -> &'static str {
        match self {
            ExportFormat::Json => " analysis",
            ExportFormat::PitchCsv => " pitch",
            ExportFormat::NotesCsv => " notes",
            ExportFormat::Midi => " notes",
            ExportFormat::Report => " report",
        }
    }
}

/// The other recording in an A/B comparison, as it appears in an export.
pub struct ComparedWith<'a> {
    pub title: String,
    /// The exported recording is the reference (first) of the pair.
    pub exported_is_reference: bool,
    pub alignment: Option<Alignment>,
    pub pitch: Option<&'a PitchComparison>,
}

/// Everything an export describes.
pub struct ExportContext<'a> {
    pub recording: &'a Recording,
    pub analysis: &'a Analysis,
    /// The analysis was made from the isolated vocals.
    pub isolated_vocals: bool,
    /// Name of the model that isolated them.
    pub isolation_model: Option<String>,
    pub compared_with: Option<ComparedWith<'a>>,
    pub exported_at: Timestamp,
}

impl ExportContext<'_> {
    /// Whether the figures deserve the full-mix caution.
    pub fn needs_caution(&self) -> bool {
        !self.isolated_vocals && self.recording.source.kind != SourceKind::VocalStem
    }
}

pub fn render(format: ExportFormat, context: &ExportContext) -> AppResult<Vec<u8>> {
    Ok(match format {
        ExportFormat::Json => {
            let mut text = serde_json::to_string_pretty(&json_document(context))
                .map_err(|err| AppError::Internal(format!("could not build the export: {err}")))?;
            text.push('\n');
            text.into_bytes()
        }
        ExportFormat::PitchCsv => pitch_csv(context.analysis).into_bytes(),
        ExportFormat::NotesCsv => notes_csv(context.analysis).into_bytes(),
        ExportFormat::Midi => midi_file(context),
        ExportFormat::Report => report(context).into_bytes(),
    })
}

/// Renders and writes an export atomically.
pub fn write(format: ExportFormat, context: &ExportContext, path: &Path) -> AppResult<()> {
    let bytes = render(format, context)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension(format!("{}.part", format.extension()));
    let written = (|| -> AppResult<()> {
        let mut file = std::fs::File::create(&partial)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&partial, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

/// Rounds for output: enough digits to lose nothing that was measured.
fn round(value: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (value * scale).round() / scale
}

fn json_document(context: &ExportContext) -> serde_json::Value {
    let recording = context.recording;
    let analysis = context.analysis;
    let track = &analysis.track;
    let voiced = |values: &[f32], digits: i32| -> Vec<serde_json::Value> {
        values
            .iter()
            .zip(&track.frequency_hz)
            .map(|(value, hz)| {
                if *hz > 0.0 {
                    json!(round(*value as f64, digits))
                } else {
                    serde_json::Value::Null
                }
            })
            .collect()
    };
    json!({
        "vocalscope_export_version": EXPORT_FORMAT_VERSION,
        "application_version": env!("CARGO_PKG_VERSION"),
        "exported_at": context.exported_at,
        "notice": CAVEAT,
        "recording": {
            "title": recording.display_title(),
            "file_name": recording.audio.file_name,
            "duration_seconds": recording.duration_seconds(),
            "source_kind": recording.source.kind,
            "label": recording.label,
            "audio": recording.audio,
        },
        "analysis": {
            "input": if context.isolated_vocals { "isolated_vocals" } else { "original" },
            "isolation_model": context.isolation_model,
            "caution": context.needs_caution().then_some(FULL_MIX_CAUTION),
            "method": "YIN difference function with Viterbi path selection",
            "frame_interval_seconds": track.hop_seconds,
            "summary": analysis.summary,
            "indicators": analysis.indicators,
            "notes": analysis.notes,
            "pitch": {
                "description": "One value per frame; frame i is centred on i × frame_interval_seconds. null where there is no pitch.",
                "frequency_hz": voiced(&track.frequency_hz, 2),
                "confidence": voiced(&track.confidence, 3),
            },
        },
        "comparison": context.compared_with.as_ref().map(|compared| json!({
            "compared_with": compared.title,
            "this_recording_is_the_reference": compared.exported_is_reference,
            "alignment": compared.alignment,
            "alignment_description": "other_seconds = offset_seconds + speed_ratio × reference_seconds",
            "pitch": compared.pitch,
            "pitch_description": "Differences are the other recording minus the reference, in cents, on the reference's timeline.",
        })),
    })
}

fn pitch_csv(analysis: &Analysis) -> String {
    let track = &analysis.track;
    let mut out = String::from(
        "time_seconds,frequency_hz,midi_pitch,note,deviation_cents,confidence,level_db\n",
    );
    for i in 0..track.len() {
        let time = i as f64 * track.hop_seconds;
        let hz = track.frequency_hz[i];
        let level = track.level_db[i];
        let level = if level.is_finite() {
            format!("{level:.1}")
        } else {
            String::new()
        };
        if hz > 0.0 {
            let midi = hz_to_midi(hz);
            let (name, cents) = describe_pitch(midi);
            let _ = writeln!(
                out,
                "{time:.2},{hz:.2},{midi:.3},{name},{cents:.1},{:.3},{level}",
                track.confidence[i]
            );
        } else {
            let _ = writeln!(out, "{time:.2},,,,,,{level}");
        }
    }
    out
}

fn notes_csv(analysis: &Analysis) -> String {
    let optional = |value: Option<f32>, digits: usize| {
        value.map_or(String::new(), |v| format!("{v:.digits$}"))
    };
    let mut out = String::from(
        "start_seconds,end_seconds,duration_seconds,note,midi_note,frequency_hz,deviation_cents,steadiness_cents,drift_cents,vibrato_rate_hz,vibrato_extent_cents,transition_in_ms,level_db\n",
    );
    for note in &analysis.notes {
        let _ = writeln!(
            out,
            "{:.2},{:.2},{:.2},{},{},{:.2},{:.1},{},{},{},{},{},{:.1}",
            note.start_seconds,
            note.end_seconds,
            note.duration_seconds(),
            note.name,
            note.midi_note,
            note.frequency_hz,
            note.deviation_cents,
            optional(note.steadiness_cents, 1),
            optional(note.drift_cents, 1),
            optional(note.vibrato.map(|v| v.rate_hz), 2),
            optional(note.vibrato.map(|v| v.extent_cents), 1),
            optional(note.transition_in_ms, 0),
            note.level_db,
        );
    }
    out
}

/// MIDI variable-length quantity.
fn push_variable(bytes: &mut Vec<u8>, mut value: u32) {
    let mut stack = [0u8; 5];
    let mut count = 0;
    loop {
        stack[count] = (value & 0x7f) as u8;
        count += 1;
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    for i in (0..count).rev() {
        bytes.push(stack[i] | if i > 0 { 0x80 } else { 0 });
    }
}

/// A single-track Standard MIDI File of the notes. Times are exact: one
/// second of the recording is 960 ticks.
fn midi_file(context: &ExportContext) -> Vec<u8> {
    let mut track: Vec<u8> = Vec::new();
    let name = context.recording.display_title();
    let name = &name.as_bytes()[..name.len().min(120)];
    track.extend([0x00, 0xff, 0x03, name.len() as u8]);
    track.extend(name);
    track.extend([0x00, 0xff, 0x51, 0x03]);
    track.extend(&MIDI_MICROSECONDS_PER_QUARTER.to_be_bytes()[1..]);

    let mut now = 0u32;
    for (index, note) in context.analysis.notes.iter().enumerate() {
        let start = ((note.start_seconds * MIDI_TICKS_PER_SECOND).round() as u32).max(now);
        let mut end = (note.end_seconds * MIDI_TICKS_PER_SECOND).round() as u32;
        // One voice: a note never runs into the next.
        if let Some(next) = context.analysis.notes.get(index + 1) {
            end = end.min((next.start_seconds * MIDI_TICKS_PER_SECOND).round() as u32);
        }
        let end = end.max(start + 1);
        // −42 dBFS and below is as soft as MIDI goes; −6 and above is full.
        let velocity = (((note.level_db + 42.0) / 36.0).clamp(0.0, 1.0) * 126.0) as u8 + 1;
        let key = note.midi_note.min(127);
        push_variable(&mut track, start - now);
        track.extend([0x90, key, velocity]);
        push_variable(&mut track, end - start);
        track.extend([0x80, key, 0]);
        now = end;
    }
    track.extend([0x00, 0xff, 0x2f, 0x00]);

    let mut file = Vec::with_capacity(track.len() + 22);
    file.extend(b"MThd");
    file.extend(6u32.to_be_bytes());
    file.extend(0u16.to_be_bytes()); // format 0
    file.extend(1u16.to_be_bytes()); // one track
    file.extend(MIDI_TICKS_PER_QUARTER.to_be_bytes());
    file.extend(b"MTrk");
    file.extend((track.len() as u32).to_be_bytes());
    file.extend(track);
    file
}

/// Escapes the characters that would change a Markdown table or line.
fn plain(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn describe_alignment(alignment: &Alignment) -> String {
    let quality = match alignment.quality {
        AlignmentQuality::Good => "The two recordings line up clearly",
        AlignmentQuality::Uncertain => "The two recordings probably line up; check by ear",
        AlignmentQuality::Poor => {
            "No convincing match was found; these may not be the same performance"
        }
    };
    let speed = if alignment.speed_ratio == 1.0 {
        "the same speed".to_string()
    } else {
        format!(
            "{:.3}% {}",
            (alignment.speed_ratio - 1.0).abs() * 100.0,
            if alignment.speed_ratio > 1.0 {
                "slower"
            } else {
                "faster"
            }
        )
    };
    format!(
        "{quality} (similarity {:.0}%). The other recording starts {:.3} s {} and runs at {speed}.",
        alignment.confidence * 100.0,
        alignment.offset_seconds.abs(),
        if alignment.offset_seconds >= 0.0 {
            "later"
        } else {
            "earlier"
        },
    )
}

fn report(context: &ExportContext) -> String {
    let recording = context.recording;
    let analysis = context.analysis;
    let summary = &analysis.summary;
    let report = &analysis.indicators;
    let dash = "—".to_string();
    let mut out = String::new();

    let _ = writeln!(
        out,
        "# VocalScope report: {}\n",
        plain(&recording.display_title())
    );
    let _ = writeln!(
        out,
        "Exported {} by VocalScope {}.\n",
        context.exported_at.format("%Y-%m-%d %H:%M UTC"),
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(out, "> {CAVEAT}\n");

    let _ = writeln!(out, "## Recording\n");
    let audio = &recording.audio;
    let _ = writeln!(out, "- File: {}", plain(&audio.file_name));
    let _ = writeln!(
        out,
        "- Format: {}, {} Hz, {}",
        audio.codec_description,
        audio.sample_rate_hz,
        match audio.channel_count {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n} channels"),
        }
    );
    if let Some(duration) = recording.duration_seconds() {
        let _ = writeln!(out, "- Duration: {}", format_time(duration, 1));
    }
    if let Some(version) = &recording.label.version {
        let _ = writeln!(out, "- Version: {}", plain(version));
    }
    if let Some(year) = recording.label.release_year {
        let _ = writeln!(out, "- Release year: {year}");
    }
    if let Some(kind) = recording.source.kind.description() {
        let _ = writeln!(out, "- Described as: {kind}");
    }
    if let Some(notes) = &recording.label.notes {
        let _ = writeln!(out, "- Notes: {}", plain(notes));
    }
    let _ = writeln!(
        out,
        "- Analysed: {}",
        match (&context.isolation_model, context.isolated_vocals) {
            (Some(model), true) => format!("the vocals isolated by VocalScope ({model})"),
            (None, true) => "the vocals isolated by VocalScope".to_string(),
            _ => "the recording as it is".to_string(),
        }
    );
    if context.needs_caution() {
        let _ = writeln!(out, "\n{FULL_MIX_CAUTION}");
    }

    let _ = writeln!(out, "\n## Pitch\n");
    let _ = writeln!(
        out,
        "- Time with a detectable pitch: {}",
        format_time(summary.voiced_seconds, 1)
    );
    let _ = writeln!(out, "- Notes found: {}", summary.note_count);
    if let (Some(low), Some(high)) = (&summary.lowest_note, &summary.highest_note) {
        let _ = writeln!(out, "- Range: {low} to {high}");
    }
    if let (Some(note), Some(hz)) = (&summary.median_note, summary.median_frequency_hz) {
        let _ = writeln!(out, "- Median pitch: {note} ({hz:.1} Hz)");
    }
    if let (Some(offset), Some(reference)) =
        (summary.tuning_offset_cents, summary.reference_pitch_hz)
    {
        let _ = writeln!(
            out,
            "- Overall tuning: {offset:+.0} cents from standard pitch (A4 ≈ {reference:.1} Hz)"
        );
    }

    let _ = writeln!(out, "\n## Pitch-correction indicators\n");
    let _ = writeln!(out, "**{}.** {}\n", report.headline, report.summary);
    let _ = writeln!(out, "| Indicator | Measurement | Reading |");
    let _ = writeln!(out, "| --- | --- | --- |");
    for indicator in &report.indicators {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            indicator.title,
            indicator.display_value,
            indicator.assessment.label()
        );
    }
    let _ = writeln!(out);
    for indicator in &report.indicators {
        let _ = writeln!(out, "- **{}.** {}", indicator.title, indicator.explanation);
    }
    let _ = writeln!(out, "\n{}", report.caveat);

    if let Some(compared) = &context.compared_with {
        let _ = writeln!(out, "\n## Comparison with {}\n", plain(&compared.title));
        match &compared.alignment {
            Some(alignment) => {
                let _ = writeln!(out, "{}\n", describe_alignment(alignment));
            }
            None => {
                let _ = writeln!(out, "The two recordings could not be lined up.\n");
            }
        }
        if let Some(pitch) = compared.pitch {
            let _ = writeln!(
                out,
                "- Time compared (both have a pitch): {}",
                format_time(pitch.compared_seconds, 1)
            );
            if let Some(shift) = pitch.median_difference_cents {
                let _ = writeln!(
                    out,
                    "- Overall pitch difference: {shift:+.1} cents (other minus reference)"
                );
            }
            if let Some(typical) = pitch.typical_difference_cents {
                let _ = writeln!(
                    out,
                    "- Typical difference once that is removed: {typical:.1} cents"
                );
            }
            if let Some(share) = pitch.share_within_10_cents {
                let _ = writeln!(
                    out,
                    "- Agreeing within 10 cents: {:.0}% of the time",
                    share * 100.0
                );
            }
            if pitch.regions.is_empty() {
                let _ = writeln!(out, "- No passages differ by more than 25 cents.");
            } else {
                let _ = writeln!(
                    out,
                    "\nPassages that differ by more than 25 cents ({}; times on the reference recording):\n",
                    pitch.regions.len()
                );
                let _ = writeln!(out, "| From | To | Difference |");
                let _ = writeln!(out, "| --- | --- | --- |");
                for region in pitch.regions.iter().take(REPORT_MAX_REGIONS) {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {:+.0} cents |",
                        format_time(region.start_seconds, 2),
                        format_time(region.end_seconds, 2),
                        region.mean_difference_cents
                    );
                }
                if pitch.regions.len() > REPORT_MAX_REGIONS {
                    let _ = writeln!(
                        out,
                        "\n…and {} more; the JSON export lists them all.",
                        pitch.regions.len() - REPORT_MAX_REGIONS
                    );
                }
            }
        }
    }

    if !analysis.notes.is_empty() {
        let _ = writeln!(out, "\n## Notes\n");
        let _ = writeln!(
            out,
            "| Start | Length | Note | From the scale | Steadiness | Vibrato |"
        );
        let _ = writeln!(out, "| --- | --- | --- | --- | --- | --- |");
        for note in analysis.notes.iter().take(REPORT_MAX_NOTES) {
            let _ = writeln!(
                out,
                "| {} | {:.2} s | {} | {:+.0} cents | {} | {} |",
                format_time(note.start_seconds, 2),
                note.duration_seconds(),
                note.name,
                note.deviation_cents,
                note.steadiness_cents
                    .map_or(dash.clone(), |v| format!("{v:.1} cents")),
                note.vibrato.map_or(dash.clone(), |v| format!(
                    "{:.1} Hz, ±{:.0} cents",
                    v.rate_hz, v.extent_cents
                )),
            );
        }
        if analysis.notes.len() > REPORT_MAX_NOTES {
            let _ = writeln!(
                out,
                "\n…and {} more; the notes CSV export lists them all.",
                analysis.notes.len() - REPORT_MAX_NOTES
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::analysis::compare::DifferenceRegion;
    use crate::analysis::pitch::test_support::track;
    use crate::analysis::test_support::phrase;
    use crate::project::test_support::audio_info;
    use crate::project::RecordingLabel;

    struct Fixture {
        recording: Recording,
        analysis: Analysis,
        comparison: PitchComparison,
    }

    fn fixture() -> Fixture {
        let mut recording = Recording::new(Path::new("/music/take.wav"), audio_info("take.wav"));
        recording.label = RecordingLabel {
            recording_name: Some("Lead | vocal".into()),
            version: Some("2011 remaster".into()),
            release_year: Some(2011),
            notes: Some("First line\nsecond line".into()),
        };
        Fixture {
            recording,
            analysis: Analysis::from_track(track(44_100, &phrase(44_100))),
            comparison: PitchComparison {
                compared_seconds: 2.5,
                median_difference_cents: Some(3.5),
                typical_difference_cents: Some(4.0),
                share_within_10_cents: Some(0.82),
                regions: vec![DifferenceRegion {
                    start_seconds: 1.0,
                    end_seconds: 1.4,
                    mean_difference_cents: -38.0,
                }],
            },
        }
    }

    fn context(f: &Fixture, compare: bool) -> ExportContext<'_> {
        ExportContext {
            recording: &f.recording,
            analysis: &f.analysis,
            isolated_vocals: false,
            isolation_model: None,
            compared_with: compare.then(|| ComparedWith {
                title: "Original release".into(),
                exported_is_reference: true,
                alignment: Some(Alignment {
                    offset_seconds: 1.25,
                    speed_ratio: 1.002,
                    confidence: 0.91,
                    quality: AlignmentQuality::Good,
                }),
                pitch: Some(&f.comparison),
            }),
            exported_at: chrono::Utc
                .with_ymd_and_hms(2026, 10, 6, 12, 30, 0)
                .unwrap(),
        }
    }

    fn text(format: ExportFormat, context: &ExportContext) -> String {
        String::from_utf8(render(format, context).unwrap()).unwrap()
    }

    #[test]
    fn json_holds_everything_under_spelled_out_names() {
        let f = fixture();
        let json: serde_json::Value =
            serde_json::from_str(&text(ExportFormat::Json, &context(&f, true))).unwrap();
        assert_eq!(json["vocalscope_export_version"], 1);
        assert_eq!(json["notice"], CAVEAT);
        assert_eq!(json["recording"]["title"], "Lead | vocal");
        assert_eq!(json["recording"]["audio"]["sample_rate_hz"], 44_100);
        assert_eq!(json["recording"]["label"]["release_year"], 2011);

        let analysis = &json["analysis"];
        assert_eq!(analysis["input"], "original");
        assert_eq!(analysis["caution"], FULL_MIX_CAUTION);
        assert_eq!(analysis["frame_interval_seconds"], 0.01);
        assert_eq!(analysis["summary"]["note_count"], 5);
        assert_eq!(analysis["notes"].as_array().unwrap().len(), 5);
        assert_eq!(analysis["notes"][0]["name"], "C4");
        assert_eq!(analysis["indicators"]["indicators"][0]["id"], "scale_fit");
        assert_eq!(analysis["indicators"]["caveat"], CAVEAT);
        let pitch = analysis["pitch"]["frequency_hz"].as_array().unwrap();
        assert_eq!(pitch.len(), f.analysis.track.len());
        assert!((pitch[20].as_f64().unwrap() - 261.63).abs() < 0.1);
        assert!(pitch[160].is_null(), "the breath has no pitch");
        assert_eq!(
            analysis["pitch"]["confidence"].as_array().unwrap().len(),
            pitch.len()
        );

        let comparison = &json["comparison"];
        assert_eq!(comparison["compared_with"], "Original release");
        assert_eq!(comparison["alignment"]["quality"], "good");
        assert_eq!(comparison["alignment"]["speed_ratio"], 1.002);
        assert_eq!(
            comparison["pitch"]["regions"][0]["mean_difference_cents"],
            -38.0
        );

        let alone: serde_json::Value =
            serde_json::from_str(&text(ExportFormat::Json, &context(&f, false))).unwrap();
        assert!(alone["comparison"].is_null());
    }

    #[test]
    fn pitch_csv_has_one_row_per_frame() {
        let f = fixture();
        let csv = text(ExportFormat::PitchCsv, &context(&f, false));
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "time_seconds,frequency_hz,midi_pitch,note,deviation_cents,confidence,level_db"
        );
        assert_eq!(lines.len(), f.analysis.track.len() + 1);
        let voiced: Vec<&str> = lines[21].split(',').collect();
        assert_eq!(voiced.len(), 7);
        assert_eq!(voiced[0], "0.20");
        assert!((voiced[1].parse::<f64>().unwrap() - 261.63).abs() < 0.1);
        assert_eq!(voiced[3], "C4");
        let unvoiced: Vec<&str> = lines[161].split(',').collect();
        assert_eq!(unvoiced.len(), 7);
        assert_eq!(unvoiced[0], "1.60");
        assert!(unvoiced[1..6].iter().all(|field| field.is_empty()));
    }

    #[test]
    fn notes_csv_has_one_row_per_note() {
        let f = fixture();
        let csv = text(ExportFormat::NotesCsv, &context(&f, false));
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(lines[0].starts_with("start_seconds,end_seconds,duration_seconds,note,midi_note"));
        let columns = lines[0].split(',').count();
        assert!(lines.iter().all(|line| line.split(',').count() == columns));
        let second: Vec<&str> = lines[2].split(',').collect();
        assert_eq!(second[3], "D4");
        assert_eq!(second[4], "62");
        // Joined to the note before it, so its arrival was timed.
        assert!(!second[11].is_empty());
        let first: Vec<&str> = lines[1].split(',').collect();
        assert!(first[11].is_empty());
    }

    /// Reads a variable-length quantity; returns (value, bytes used).
    fn read_variable(bytes: &[u8]) -> (u32, usize) {
        let mut value = 0u32;
        for (i, byte) in bytes.iter().enumerate() {
            value = (value << 7) | (byte & 0x7f) as u32;
            if byte & 0x80 == 0 {
                return (value, i + 1);
            }
        }
        panic!("unterminated quantity");
    }

    #[test]
    fn midi_is_a_valid_single_track_file_with_exact_times() {
        let f = fixture();
        let bytes = render(ExportFormat::Midi, &context(&f, false)).unwrap();
        assert_eq!(&bytes[..4], b"MThd");
        assert_eq!(&bytes[4..14], [0, 0, 0, 6, 0, 0, 0, 1, 0x01, 0xe0]);
        assert_eq!(&bytes[14..18], b"MTrk");
        let length = u32::from_be_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]) as usize;
        assert_eq!(bytes.len(), 22 + length);
        assert_eq!(&bytes[bytes.len() - 4..], [0x00, 0xff, 0x2f, 0x00]);

        // Walk the events and collect (tick, on/off, key).
        let track = &bytes[22..];
        let mut at = 0usize;
        let mut tick = 0u32;
        let mut events = Vec::new();
        while at < track.len() {
            let (delta, used) = read_variable(&track[at..]);
            at += used;
            tick += delta;
            match track[at] {
                0xff => {
                    let (len, used) = read_variable(&track[at + 2..]);
                    at += 2 + used + len as usize;
                }
                status @ (0x90 | 0x80) => {
                    events.push((tick, status == 0x90, track[at + 1], track[at + 2]));
                    at += 3;
                }
                other => panic!("unexpected status byte {other:#x}"),
            }
        }
        assert_eq!(events.len(), 10);
        let keys: Vec<u8> = events.iter().filter(|e| e.1).map(|e| e.2).collect();
        assert_eq!(keys, [60, 62, 64, 67, 64]);
        // The fourth note starts 1.75 s in: after the phrase and the breath.
        let fourth = events.iter().filter(|e| e.1).nth(3).unwrap();
        assert!(
            (fourth.0 as f64 / 960.0 - 1.75).abs() < 0.03,
            "tick {}",
            fourth.0
        );
        assert!((1..=127).contains(&fourth.3));
        assert!(events.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    }

    #[test]
    fn variable_length_quantities_follow_the_midi_standard() {
        for (value, expected) in [
            (0u32, vec![0x00]),
            (0x7f, vec![0x7f]),
            (0x80, vec![0x81, 0x00]),
            (0x3fff, vec![0xff, 0x7f]),
            (0x4000, vec![0x81, 0x80, 0x00]),
            (0x0fff_ffff, vec![0xff, 0xff, 0xff, 0x7f]),
        ] {
            let mut bytes = Vec::new();
            push_variable(&mut bytes, value);
            assert_eq!(bytes, expected, "{value:#x}");
        }
    }

    #[test]
    fn the_report_reads_plainly_and_keeps_its_caveats() {
        let f = fixture();
        let report = text(ExportFormat::Report, &context(&f, true));
        assert!(report.starts_with("# VocalScope report: Lead \\| vocal\n"));
        assert!(report.contains("Exported 2026-10-06 12:30 UTC by VocalScope"));
        assert!(report.contains(&format!("> {CAVEAT}")));
        assert!(report.contains("- Version: 2011 remaster"));
        assert!(report.contains("- Notes: First line second line"));
        assert!(report.contains("- Analysed: the recording as it is"));
        assert!(report.contains(FULL_MIX_CAUTION));
        assert!(report.contains("- Notes found: 5"));
        assert!(report.contains("- Range: C4 to G4"));
        assert!(report.contains("| Closeness to the scale |"));
        assert!(report.contains("## Comparison with Original release"));
        assert!(report.contains("starts 1.250 s later and runs at 0.200% slower"));
        assert!(report.contains("| 0:01.00 | 0:01.40 | -38 cents |"));
        assert!(report.contains("| 0:00.00 |"));
        // The caveat appears at the top and again under the indicators.
        assert_eq!(report.matches(CAVEAT).count(), 2);

        let mut vocals = context(&f, false);
        vocals.isolated_vocals = true;
        vocals.isolation_model = Some("Kim Vocal 2".into());
        let report = text(ExportFormat::Report, &vocals);
        assert!(report.contains("- Analysed: the vocals isolated by VocalScope (Kim Vocal 2)"));
        assert!(!report.contains(FULL_MIX_CAUTION));
        assert!(!report.contains("## Comparison"));
    }

    #[test]
    fn a_stem_supplied_by_the_user_needs_no_caution() {
        let mut f = fixture();
        assert!(context(&f, false).needs_caution());
        f.recording.source.kind = SourceKind::VocalStem;
        assert!(!context(&f, false).needs_caution());
    }

    #[test]
    fn exports_are_written_whole_or_not_at_all() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out").join("take notes.csv");
        write(ExportFormat::NotesCsv, &context(&f, false), &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 6);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );

        // A folder in the way of the file.
        let blocked = dir.path().join("blocked.mid");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write(ExportFormat::Midi, &context(&f, false), &blocked).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn formats_name_their_files() {
        assert_eq!(ExportFormat::Json.extension(), "json");
        assert_eq!(ExportFormat::PitchCsv.extension(), "csv");
        assert_eq!(ExportFormat::Midi.extension(), "mid");
        assert_eq!(ExportFormat::Report.extension(), "md");
        assert_ne!(
            ExportFormat::PitchCsv.name_suffix(),
            ExportFormat::NotesCsv.name_suffix()
        );
    }
}
