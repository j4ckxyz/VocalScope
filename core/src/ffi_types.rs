//! How types that are not native to the FFI cross the language boundary.
//!
//! * `Uuid` travels as its canonical string.
//! * Paths travel as strings (each platform's UI turns them into its own URL
//!   or path type).
//! * Timestamps travel as the platform's native date type.

use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A point in time, UTC. Named so the FFI can attach a conversion to it.
pub type Timestamp = DateTime<Utc>;

uniffi::custom_type!(Uuid, String, {
    remote,
    try_lift: |text| Ok(Uuid::parse_str(&text)?),
    lower: |id| id.to_string(),
});

uniffi::custom_type!(PathBuf, String, {
    remote,
    try_lift: |text| Ok(PathBuf::from(text)),
    lower: |path| path.to_string_lossy().into_owned(),
});

uniffi::custom_type!(Timestamp, SystemTime, {
    remote,
    try_lift: |time| Ok(Timestamp::from(time)),
    lower: |timestamp| SystemTime::from(timestamp),
});
