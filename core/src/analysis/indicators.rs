//! Pitch-correction indicators.
//!
//! Four measurements of how the sung pitch behaves, each compared against
//! what unprocessed singing usually looks like. They are indicators and
//! estimates. None of them, alone or together, can prove that a particular
//! tool was or was not used: a very accurate singer and light correction can
//! look alike, and heavy correction can be hidden by later processing. Every
//! piece of text produced here is written to say so.
//!
//! The thresholds are deliberately conservative rules of thumb, kept
//! together at the top of this file so they are easy to review and revise.

use serde::Serialize;

use super::notes::Note;

/// Notes shorter than this are left out of the scale-fit measurement; their
/// centre is too influenced by the way in and out of them.
const SCALE_FIT_MIN_SECONDS: f64 = 0.15;
const SCALE_FIT_MIN_NOTES: usize = 8;
/// Median distance of note centres from the scale, in cents.
const SCALE_FIT_CORRECTED_BELOW: f64 = 6.0;
const SCALE_FIT_NATURAL_ABOVE: f64 = 12.0;

const STEADINESS_MIN_NOTES: usize = 5;
/// Median wander within sustained notes, in cents.
const STEADINESS_CORRECTED_BELOW: f64 = 3.0;
const STEADINESS_NATURAL_ABOVE: f64 = 6.0;

/// Notes long enough for vibrato to develop.
const VIBRATO_MIN_SECONDS: f64 = 0.4;
const VIBRATO_MIN_NOTES: usize = 3;
const VIBRATO_NATURAL_SHARE: f64 = 0.3;
const VIBRATO_NATURAL_EXTENT_CENTS: f64 = 20.0;

const TRANSITION_MIN_COUNT: usize = 5;
/// Median time for the middle 60% of a move between notes, in milliseconds.
const TRANSITION_CORRECTED_BELOW: f64 = 18.0;
const TRANSITION_NATURAL_ABOVE: f64 = 35.0;

pub const CAVEAT: &str = "These are estimates from the pitch curve alone. They cannot prove whether pitch correction or any particular tool was used: a very accurate singer and light correction can look the same.";

/// How one measurement compares with unprocessed singing.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum Assessment {
    /// Too little suitable material to measure.
    NotEnoughData,
    /// In the range usually seen in unprocessed singing.
    TypicalOfUnprocessed,
    /// Between the two, or not informative either way.
    Inconclusive,
    /// In the range usually seen only after pitch correction.
    ConsistentWithCorrection,
}

impl Assessment {
    pub fn label(self) -> &'static str {
        match self {
            Assessment::NotEnoughData => "Not enough data",
            Assessment::TypicalOfUnprocessed => "Typical of unprocessed singing",
            Assessment::Inconclusive => "Inconclusive",
            Assessment::ConsistentWithCorrection => "Consistent with pitch correction",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct Indicator {
    /// Stable identifier: `scale_fit`, `steadiness`, `vibrato`, `transitions`.
    pub id: String,
    pub title: String,
    /// The measurement, in `unit`. `None` when there was too little to measure.
    pub value: Option<f64>,
    pub unit: String,
    /// The measurement as text, e.g. `4.2 cents`.
    pub display_value: String,
    pub assessment: Assessment,
    /// What was measured and how to read it, in plain language.
    pub explanation: String,
    /// How many notes or transitions the measurement rests on.
    pub sample_count: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct IndicatorReport {
    pub indicators: Vec<Indicator>,
    pub overall: Assessment,
    /// One line, e.g. "The indicators are mixed".
    pub headline: String,
    pub summary: String,
    /// Always shown with the report.
    pub caveat: String,
}

/// How far the recording as a whole sits from A4 = 440 Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Offset of the scale the singer is closest to, in cents (−50 to 50).
    pub offset_cents: f64,
    /// 0 (note centres spread evenly between semitones) to 1 (all centres
    /// the same distance from the scale).
    pub concentration: f64,
}

/// Wraps a distance in cents into −50..50.
fn wrap_cents(cents: f64) -> f64 {
    cents - 100.0 * (cents / 100.0).round()
}

/// Estimates the tuning reference from where note centres cluster between
/// semitones: a circular mean, weighted by note length.
pub fn estimate_tuning(notes: &[Note]) -> Option<Tuning> {
    let (mut x, mut y, mut weight) = (0f64, 0f64, 0f64);
    let mut count = 0usize;
    for note in notes {
        let duration = note.duration_seconds();
        if duration < SCALE_FIT_MIN_SECONDS {
            continue;
        }
        let angle = note.deviation_cents as f64 / 100.0 * std::f64::consts::TAU;
        x += duration * angle.cos();
        y += duration * angle.sin();
        weight += duration;
        count += 1;
    }
    if count < 3 || weight <= 0.0 {
        return None;
    }
    Some(Tuning {
        offset_cents: y.atan2(x) / std::f64::consts::TAU * 100.0,
        concentration: (x * x + y * y).sqrt() / weight,
    })
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

/// Lower values point towards correction.
fn grade(value: f64, corrected_below: f64, natural_above: f64) -> Assessment {
    if value < corrected_below {
        Assessment::ConsistentWithCorrection
    } else if value > natural_above {
        Assessment::TypicalOfUnprocessed
    } else {
        Assessment::Inconclusive
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn scale_fit(notes: &[Note], tuning: Option<Tuning>) -> Indicator {
    let title = "Closeness to the scale".to_string();
    let offset = tuning.map_or(0.0, |t| t.offset_cents);
    let distances: Vec<f64> = notes
        .iter()
        .filter(|note| note.duration_seconds() >= SCALE_FIT_MIN_SECONDS)
        .map(|note| wrap_cents(note.deviation_cents as f64 - offset).abs())
        .collect();
    let count = distances.len();
    let value = median(distances).filter(|_| count >= SCALE_FIT_MIN_NOTES);
    let Some(value) = value else {
        return Indicator {
            id: "scale_fit".into(),
            title,
            value: None,
            unit: "cents".into(),
            display_value: "—".into(),
            assessment: Assessment::NotEnoughData,
            explanation: format!(
                "Needs at least {SCALE_FIT_MIN_NOTES} notes of {} ms or longer; found {count}.",
                (SCALE_FIT_MIN_SECONDS * 1000.0) as u32
            ),
            sample_count: count as u32,
        };
    };
    let assessment = grade(value, SCALE_FIT_CORRECTED_BELOW, SCALE_FIT_NATURAL_ABOVE);
    let reading = match assessment {
        Assessment::ConsistentWithCorrection => {
            "Note centres sit unusually close to exact semitones. Unprocessed singing, even when very good, usually lands 10 to 25 cents away."
        }
        Assessment::TypicalOfUnprocessed => {
            "Note centres are spread around the scale by an amount that is usual for unprocessed singing."
        }
        _ => "Note centres are close to the scale, within what an accurate singer can do unaided.",
    };
    Indicator {
        id: "scale_fit".into(),
        title,
        value: Some(value),
        unit: "cents".into(),
        display_value: format!("{value:.1} cents"),
        assessment,
        explanation: format!(
            "Typical distance of a note's centre from the nearest semitone, across {}, after allowing for the recording's overall tuning. {reading}",
            plural(count, "note", "notes")
        ),
        sample_count: count as u32,
    }
}

fn steadiness(notes: &[Note]) -> Indicator {
    let title = "Steadiness of held notes".to_string();
    let values: Vec<f64> = notes
        .iter()
        .filter_map(|note| note.steadiness_cents.map(f64::from))
        .collect();
    let count = values.len();
    let value = median(values).filter(|_| count >= STEADINESS_MIN_NOTES);
    let Some(value) = value else {
        return Indicator {
            id: "steadiness".into(),
            title,
            value: None,
            unit: "cents".into(),
            display_value: "—".into(),
            assessment: Assessment::NotEnoughData,
            explanation: format!(
                "Needs at least {STEADINESS_MIN_NOTES} held notes of a quarter of a second or longer; found {count}."
            ),
            sample_count: count as u32,
        };
    };
    let assessment = grade(value, STEADINESS_CORRECTED_BELOW, STEADINESS_NATURAL_ABOVE);
    let reading = match assessment {
        Assessment::ConsistentWithCorrection => {
            "Held notes are almost perfectly flat. A voice normally wanders by several cents even on a steady note."
        }
        Assessment::TypicalOfUnprocessed => {
            "Held notes wander by an amount that is usual for an unprocessed voice."
        }
        _ => "Held notes are steadier than most unprocessed singing but not unnaturally so.",
    };
    Indicator {
        id: "steadiness".into(),
        title,
        value: Some(value),
        unit: "cents".into(),
        display_value: format!("{value:.1} cents"),
        assessment,
        explanation: format!(
            "Typical irregular pitch movement within the settled part of {}, not counting vibrato or steady drift. {reading}",
            plural(count, "held note", "held notes")
        ),
        sample_count: count as u32,
    }
}

fn vibrato(notes: &[Note]) -> Indicator {
    let title = "Vibrato".to_string();
    let long: Vec<&Note> = notes
        .iter()
        .filter(|note| note.duration_seconds() >= VIBRATO_MIN_SECONDS)
        .collect();
    let count = long.len();
    if count < VIBRATO_MIN_NOTES {
        return Indicator {
            id: "vibrato".into(),
            title,
            value: None,
            unit: "cents".into(),
            display_value: "—".into(),
            assessment: Assessment::NotEnoughData,
            explanation: format!(
                "Needs at least {VIBRATO_MIN_NOTES} notes of {} ms or longer; found {count}.",
                (VIBRATO_MIN_SECONDS * 1000.0) as u32
            ),
            sample_count: count as u32,
        };
    }
    let with: Vec<_> = long.iter().filter_map(|note| note.vibrato).collect();
    let rate = median(with.iter().map(|v| v.rate_hz as f64).collect());
    let extent = median(with.iter().map(|v| v.extent_cents as f64).collect());
    let share = with.len() as f64 / count as f64;

    let (display_value, assessment, reading) = match (rate, extent) {
        (Some(rate), Some(extent)) => {
            let natural = share >= VIBRATO_NATURAL_SHARE && extent >= VIBRATO_NATURAL_EXTENT_CENTS;
            (
                format!("{rate:.1} Hz, ±{extent:.0} cents"),
                if natural {
                    Assessment::TypicalOfUnprocessed
                } else {
                    Assessment::Inconclusive
                },
                if natural {
                    "Its speed and depth are in the usual range for a singer's own vibrato, which strong correction tends to flatten."
                } else {
                    "It is shallow or rare, which happens both in restrained singing and after correction."
                },
            )
        }
        _ => (
            "None found".to_string(),
            Assessment::Inconclusive,
            "Singing without vibrato is common in many styles, so its absence says little by itself.",
        ),
    };
    Indicator {
        id: "vibrato".into(),
        title,
        value: extent,
        unit: "cents".into(),
        display_value,
        assessment,
        explanation: format!(
            "Regular pitch wobble was found on {} of {}. {reading}",
            with.len(),
            plural(count, "long note", "long notes")
        ),
        sample_count: count as u32,
    }
}

fn transitions(notes: &[Note]) -> Indicator {
    let title = "Movement between notes".to_string();
    let values: Vec<f64> = notes
        .iter()
        .filter_map(|note| note.transition_in_ms.map(f64::from))
        .collect();
    let count = values.len();
    let value = median(values).filter(|_| count >= TRANSITION_MIN_COUNT);
    let Some(value) = value else {
        return Indicator {
            id: "transitions".into(),
            title,
            value: None,
            unit: "ms".into(),
            display_value: "—".into(),
            assessment: Assessment::NotEnoughData,
            explanation: format!(
                "Needs at least {TRANSITION_MIN_COUNT} moves from one note straight to another without a break; found {count}."
            ),
            sample_count: count as u32,
        };
    };
    let assessment = grade(value, TRANSITION_CORRECTED_BELOW, TRANSITION_NATURAL_ABOVE);
    let reading = match assessment {
        Assessment::ConsistentWithCorrection => {
            "The pitch jumps between notes almost instantly. A voice normally takes tens of milliseconds to slide from one note to the next."
        }
        Assessment::TypicalOfUnprocessed => {
            "The pitch slides between notes at a speed that is usual for an unprocessed voice."
        }
        _ => "The pitch moves between notes quickly, as agile singers and light correction both do.",
    };
    Indicator {
        id: "transitions".into(),
        title,
        value: Some(value),
        unit: "ms".into(),
        display_value: format!("{value:.0} ms"),
        assessment,
        explanation: format!(
            "Typical time the pitch takes to cover the middle 60% of the way between two joined notes, across {}. {reading}",
            plural(count, "move", "moves")
        ),
        sample_count: count as u32,
    }
}

/// Builds the report for a set of notes.
pub fn assess(notes: &[Note]) -> IndicatorReport {
    let tuning = estimate_tuning(notes);
    let indicators = vec![
        scale_fit(notes, tuning),
        steadiness(notes),
        vibrato(notes),
        transitions(notes),
    ];
    let measured = indicators
        .iter()
        .filter(|i| i.assessment != Assessment::NotEnoughData)
        .count();
    let corrected = indicators
        .iter()
        .filter(|i| i.assessment == Assessment::ConsistentWithCorrection)
        .count();
    let natural = indicators
        .iter()
        .filter(|i| i.assessment == Assessment::TypicalOfUnprocessed)
        .count();

    let (overall, headline) = if measured < 2 {
        (Assessment::NotEnoughData, "Not enough singing to assess")
    } else if corrected >= 2 && corrected > natural {
        (
            Assessment::ConsistentWithCorrection,
            "Several indicators are consistent with pitch correction",
        )
    } else if natural >= 2 && corrected == 0 {
        (
            Assessment::TypicalOfUnprocessed,
            "The indicators are typical of unprocessed singing",
        )
    } else {
        (Assessment::Inconclusive, "The indicators are mixed")
    };
    let summary = if measured < 2 {
        "There are too few sustained and joined notes in this recording for the indicators to mean much.".to_string()
    } else {
        format!(
            "Of {measured} indicators that could be measured, {corrected} {} consistent with pitch correction, {natural} {} typical of unprocessed singing and {} inconclusive.",
            if corrected == 1 { "is" } else { "are" },
            if natural == 1 { "is" } else { "are" },
            match measured - corrected - natural {
                1 => "1 is".to_string(),
                n => format!("{n} are"),
            },
        )
    };
    IndicatorReport {
        indicators,
        overall,
        headline: headline.to_string(),
        summary,
        caveat: CAVEAT.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::super::notes::Vibrato;
    use super::*;

    /// A plausible note; tests adjust the fields they care about.
    pub fn note(start: f64, seconds: f64, midi_pitch: f32) -> Note {
        let nearest = midi_pitch.round();
        Note {
            start_seconds: start,
            end_seconds: start + seconds,
            frequency_hz: super::super::pitch::midi_to_hz(midi_pitch),
            midi_pitch,
            midi_note: nearest as u8,
            name: super::super::notes::note_name(nearest as u8),
            deviation_cents: (midi_pitch - nearest) * 100.0,
            steadiness_cents: (seconds >= 0.25).then_some(8.0),
            drift_cents: (seconds >= 0.25).then_some(-6.0),
            vibrato: None,
            transition_in_ms: None,
            level_db: -18.0,
        }
    }

    /// Deviations from the scale (cents) that a good unprocessed singer
    /// might produce.
    pub const NATURAL_DEVIATIONS: [f32; 12] = [
        14.0, -22.0, 9.0, 31.0, -17.0, -6.0, 25.0, -28.0, 12.0, -19.0, 18.0, -11.0,
    ];

    pub fn natural_notes() -> Vec<Note> {
        NATURAL_DEVIATIONS
            .iter()
            .enumerate()
            .map(|(i, deviation)| {
                let mut n = note(
                    i as f64 * 0.7,
                    0.6,
                    60.0 + (i % 5) as f32 + deviation / 100.0,
                );
                n.steadiness_cents = Some(7.0 + (i % 4) as f32 * 2.0);
                n.transition_in_ms = (i % 2 == 1).then_some(55.0 + i as f32 * 3.0);
                n.vibrato = (i % 3 != 0).then_some(Vibrato {
                    rate_hz: 5.6,
                    extent_cents: 45.0,
                });
                n
            })
            .collect()
    }

    pub fn corrected_notes() -> Vec<Note> {
        (0..12)
            .map(|i| {
                let mut n = note(
                    i as f64 * 0.7,
                    0.6,
                    60.0 + (i % 5) as f32 + [0.01, -0.02, 0.0, 0.02][i % 4],
                );
                n.steadiness_cents = Some(0.8 + (i % 3) as f32 * 0.4);
                n.drift_cents = Some(0.5);
                n.transition_in_ms = (i % 2 == 1).then_some(9.0 + (i % 3) as f32);
                n
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn by_id<'a>(report: &'a IndicatorReport, id: &str) -> &'a Indicator {
        report.indicators.iter().find(|i| i.id == id).unwrap()
    }

    #[test]
    fn natural_singing_reads_as_unprocessed() {
        let report = assess(&natural_notes());
        assert_eq!(report.overall, Assessment::TypicalOfUnprocessed);
        for id in ["scale_fit", "steadiness", "vibrato", "transitions"] {
            assert_eq!(
                by_id(&report, id).assessment,
                Assessment::TypicalOfUnprocessed,
                "{id}"
            );
        }
        assert_eq!(by_id(&report, "scale_fit").sample_count, 12);
        assert_eq!(by_id(&report, "vibrato").display_value, "5.6 Hz, ±45 cents");
        assert!(by_id(&report, "transitions").display_value.ends_with(" ms"));
        assert!(report.summary.starts_with("Of 4 indicators"));
    }

    #[test]
    fn flat_quantised_stepped_pitch_reads_as_consistent_with_correction() {
        let report = assess(&corrected_notes());
        assert_eq!(report.overall, Assessment::ConsistentWithCorrection);
        for id in ["scale_fit", "steadiness", "transitions"] {
            assert_eq!(
                by_id(&report, id).assessment,
                Assessment::ConsistentWithCorrection,
                "{id}"
            );
        }
        // No vibrato is never, on its own, held against a recording.
        let vibrato = by_id(&report, "vibrato");
        assert_eq!(vibrato.assessment, Assessment::Inconclusive);
        assert_eq!(vibrato.display_value, "None found");
    }

    #[test]
    fn every_report_carries_the_caveat_and_never_claims_proof() {
        for notes in [natural_notes(), corrected_notes(), Vec::new()] {
            let report = assess(&notes);
            assert_eq!(report.caveat, CAVEAT);
            let mut text = format!("{} {}", report.headline, report.summary);
            for indicator in &report.indicators {
                text.push_str(&indicator.explanation);
            }
            let text = text.to_lowercase();
            for word in [
                "proves",
                "proof",
                "definitely",
                "auto-tune",
                "autotune",
                "certain",
            ] {
                assert!(!text.contains(word), "report text contains “{word}”");
            }
        }
        assert!(CAVEAT.contains("cannot prove"));
    }

    #[test]
    fn too_little_material_is_reported_as_such() {
        let report = assess(&natural_notes()[..2]);
        assert_eq!(report.overall, Assessment::NotEnoughData);
        assert!(report
            .indicators
            .iter()
            .all(|i| i.assessment == Assessment::NotEnoughData && i.value.is_none()));
        assert!(by_id(&report, "scale_fit").explanation.contains("found 2"));
        assert_eq!(assess(&[]).headline, "Not enough singing to assess");
    }

    #[test]
    fn a_recording_tuned_away_from_440_is_not_penalised() {
        // Every note 30 cents flat: a consistent offset is a tuning choice
        // (or a tape speed), not sloppiness, and should read as "on the scale".
        let notes: Vec<Note> = corrected_notes()
            .into_iter()
            .map(|mut n| {
                n.midi_pitch -= 0.3;
                n.deviation_cents -= 30.0;
                n
            })
            .collect();
        let tuning = estimate_tuning(&notes).unwrap();
        assert!((tuning.offset_cents + 30.0).abs() < 1.5, "{tuning:?}");
        assert!(tuning.concentration > 0.95);
        assert!(by_id(&assess(&notes), "scale_fit").value.unwrap() < 3.0);

        // Offsets near half a semitone wrap around correctly.
        let mut split = corrected_notes();
        for (i, n) in split.iter_mut().enumerate() {
            n.deviation_cents = if i % 2 == 0 { 48.0 } else { -49.0 };
        }
        let tuning = estimate_tuning(&split).unwrap();
        assert!(tuning.offset_cents.abs() > 48.0, "{tuning:?}");
        assert!(by_id(&assess(&split), "scale_fit").value.unwrap() < 2.5);
    }

    #[test]
    fn mixed_evidence_is_called_mixed() {
        let mut notes = natural_notes();
        for n in &mut notes {
            n.transition_in_ms = n.transition_in_ms.map(|_| 8.0);
        }
        let report = assess(&notes);
        assert_eq!(report.overall, Assessment::Inconclusive);
        assert_eq!(report.headline, "The indicators are mixed");
        assert!(report
            .summary
            .contains("1 is consistent with pitch correction"));
    }

    #[test]
    fn assessment_labels_are_plain_language() {
        assert_eq!(Assessment::Inconclusive.label(), "Inconclusive");
        assert!(Assessment::ConsistentWithCorrection
            .label()
            .starts_with("Consistent with"));
    }
}
