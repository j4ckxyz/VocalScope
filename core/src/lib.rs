//! VocalScope core.
//!
//! Everything that is not pixels lives here and is written once: decoding,
//! playback, waveform summaries, the project format, the local database,
//! hardware detection and timeline geometry. Each platform's native UI
//! (SwiftUI/AppKit on macOS, WinUI on Windows) is a thin layer over
//! [`app::AppCore`], reached through UniFFI-generated bindings.
//!
//! Layering inside the crate:
//!
//! * `audio`, `project`, `db`, `hardware`, `session`, `timeline` — plain
//!   logic, unit-tested, unaware of any UI.
//! * `app` — the application service the UIs call.
//! * `ffi_types`, plus the `uniffi` derives on public types — the language
//!   boundary.

pub mod app;
pub mod audio;
pub mod db;
pub mod error;
pub mod ffi_types;
pub mod hardware;
pub mod logging;
pub mod paths;
pub mod project;
pub mod session;
pub mod timeline;

uniffi::setup_scaffolding!();
