//! Analyses the pitch of audio files from the command line: timing, the
//! summary, and the correction indicators. For checking the tracker against
//! real material.
//!
//!     cargo run --release --example analyse -- FILE...

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use vocalscope_core::analysis::{analyse_file, Analysis};

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: analyse FILE...");
        std::process::exit(2);
    }
    for file in files {
        let started = Instant::now();
        let track = match analyse_file(Path::new(&file), &AtomicBool::new(false), |_| {}) {
            Ok(track) => track,
            Err(err) => {
                println!("{file}: {err}");
                continue;
            }
        };
        let tracked = started.elapsed().as_secs_f64();
        let seconds = track.duration_seconds();
        let started = Instant::now();
        let analysis = Analysis::from_track(track);
        let summary = &analysis.summary;
        println!(
            "{file}: {seconds:.1} s tracked in {tracked:.2} s ({:.0}x real time), notes in {:.1} ms",
            seconds / tracked,
            started.elapsed().as_secs_f64() * 1000.0
        );
        println!(
            "  {} notes, {:.1} s voiced, range {} to {}, tuning {:+.1} cents",
            summary.note_count,
            summary.voiced_seconds,
            summary.lowest_note.as_deref().unwrap_or("—"),
            summary.highest_note.as_deref().unwrap_or("—"),
            summary.tuning_offset_cents.unwrap_or(0.0)
        );
        println!("  {}", analysis.indicators.headline);
        for indicator in &analysis.indicators.indicators {
            println!(
                "    {:<26} {:<22} {}",
                indicator.title,
                indicator.display_value,
                indicator.assessment.label()
            );
        }
    }
}
