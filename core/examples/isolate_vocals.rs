//! Runs vocal isolation on one file from the command line and reports how
//! long it took. For measuring models and checking their output by ear.
//!
//!     cargo run --release --example isolate_vocals -- <audio> <model.onnx> <model id> <out.wav> [threads]

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use vocalscope_core::audio::decode::probe;
use vocalscope_core::separation::{isolate_vocals, models::find_model};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: isolate_vocals <audio> <model.onnx> <model id> <out.wav> [threads]");
        std::process::exit(2);
    }
    let model = find_model(&args[2]).expect("a known model id");
    let threads = args.get(4).and_then(|t| t.parse().ok()).unwrap_or(4);
    let duration = probe(Path::new(&args[0]))
        .ok()
        .and_then(|info| info.duration_seconds)
        .unwrap_or(0.0);

    let started = Instant::now();
    let mut last = Instant::now();
    isolate_vocals(
        Path::new(&args[0]),
        Path::new(&args[1]),
        model.parameters,
        threads,
        Path::new(&args[3]),
        &AtomicBool::new(false),
        |fraction| {
            if last.elapsed().as_secs() >= 5 {
                last = Instant::now();
                eprintln!("  {:.0}%", fraction.unwrap_or(0.0) * 100.0);
            }
        },
    )
    .expect("isolation failed");
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "{}: {duration:.1} s of audio in {elapsed:.1} s with {threads} threads ({:.2}x real time)",
        model.name,
        duration / elapsed
    );
}
