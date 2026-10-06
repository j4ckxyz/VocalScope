//! Measures how fast a file is decoded and summarised into a waveform.
//!
//!     cargo run --release -p vocalscope-core --example bench_waveform -- FILE...
//!
//! Prints one line per file: audio length, wall time and speed relative to
//! real time. `scripts/bench.sh` wraps this to also record peak memory.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use vocalscope_core::audio::decode;
use vocalscope_core::audio::peaks::compute_peaks;

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: bench_waveform FILE...");
        std::process::exit(2);
    }
    let cancel = AtomicBool::new(false);
    for file in files {
        let path = Path::new(&file);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let started = Instant::now();
        if let Err(err) = decode::probe(path) {
            println!("{name}: probe failed: {err}");
            continue;
        }
        let probe_ms = started.elapsed().as_secs_f64() * 1000.0;

        let started = Instant::now();
        match compute_peaks(path, &cancel, |_| {}) {
            Ok(peaks) => {
                let seconds = started.elapsed().as_secs_f64();
                let audio = peaks.duration_seconds();
                println!(
                    "{name}: probe {probe_ms:.1} ms, waveform {:.0} ms for {audio:.0} s of audio ({:.0}x real time)",
                    seconds * 1000.0,
                    audio / seconds
                );
            }
            Err(err) => println!("{name}: failed: {err}"),
        }
    }
}
