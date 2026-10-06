# VocalScope

A desktop application for analysing vocal recordings, built to help
investigate whether a vocal has been pitch-corrected or otherwise had its
pitch manipulated. Everything runs on your own computer; audio is never
uploaded.

VocalScope reports *indicators* and *estimates*. Pitch analysis alone cannot
prove that a particular tool such as Auto-Tune was used, and the application
never claims that it can.

**Status: v0.1.0, foundation.** What works today, on macOS:

- Open WAV, AIFF, FLAC, MP3, AAC/M4A, ALAC and Ogg Vorbis files (menu, drag
  and drop, Finder, or the command line), with technical metadata and
  embedded tags shown exactly as found
- Playback with seek, scrubbing, volume and mute
- A zoomable waveform timeline with overview, ruler and trackpad pinch/scroll
- Projects (`.vocalscope` files) holding labels and notes per recording, with
  missing-file recovery
- Recent files, settings, local logs

Not built yet: pitch tracking (v0.2), vocal isolation (v0.3), correction
indicators (v0.4), A/B comparison (v0.5), export (v0.6), and the Windows app.

## Design

One shared core, one native UI per platform — no web view anywhere.

| Part | Where | Technology |
| --- | --- | --- |
| Core: decoding, playback, waveforms, projects, storage, timeline maths | `core/` | Rust |
| macOS app | `apps/macos/` | Swift, SwiftUI + AppKit |
| Windows app | `apps/windows/` (not started) | C#, WinUI 3 |

The UIs reach the core through [UniFFI](https://mozilla.github.io/uniffi-rs/)
bindings generated from the Rust source. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Building (macOS)

Requirements: macOS 14 or later, the Xcode Command Line Tools
(`xcode-select --install`) and [Rust](https://rustup.rs). Full Xcode is not
needed.

```sh
apps/macos/build.sh                      # -> apps/macos/build/VocalScope.app
open apps/macos/build/VocalScope.app
```

## Testing

```sh
cargo test                               # the core: 83 tests
cargo test -- --ignored playback_smoke   # plays silently through the real audio device
uv run scripts/make_test_audio.py --long # synthetic test recordings (git-ignored)
scripts/bench.sh --save my-change        # launch time, memory and CPU
```

Performance budgets and current measurements are in
[docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Privacy

VocalScope makes no network connections in this version. Later versions will
use the network only to download analysis models you ask for and to check for
updates. Logs stay on your computer and never contain audio.

## License

MIT — see [LICENSE](LICENSE). Third-party components keep their own licenses;
notably the Symphonia decoders are MPL-2.0.
