//! Hardware profile of the machine VocalScope is running on.
//!
//! Used today for logs and the diagnostics panel. From v0.3.0 the same
//! profile drives the choice of vocal-separation model, at which point GPU
//! detection is added; nothing here pretends to know about the GPU yet.

use serde::Serialize;
use sysinfo::System;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct HardwareProfile {
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: Option<String>,
    pub cpu_model: String,
    pub cpu_architecture: String,
    pub logical_cpu_count: u32,
    pub physical_core_count: Option<u32>,
    pub is_apple_silicon: bool,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    /// Share of physical memory in use right now, 0.0–1.0.
    pub memory_used_fraction: f32,
    /// System-wide CPU load right now, 0–100. `None` when not sampled.
    pub cpu_usage_percent: Option<f32>,
    /// The operating system's own memory-pressure verdict, where it has one
    /// (macOS). `None` elsewhere.
    pub memory_pressure: Option<MemoryPressure>,
}

/// Collects the profile. With `sample_cpu_usage` the call blocks for about
/// 200 ms, the minimum interval over which CPU load can be measured — call it
/// off the main thread.
pub fn detect(sample_cpu_usage: bool) -> HardwareProfile {
    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu_all();

    let cpu_usage_percent = if sample_cpu_usage {
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        system.refresh_cpu_usage();
        Some(system.global_cpu_usage())
    } else {
        None
    };

    let cpu_model = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_string())
        .filter(|brand| !brand.is_empty())
        .unwrap_or_else(|| "Unknown CPU".to_string());
    let cpu_architecture = System::cpu_arch();
    let total_memory_bytes = system.total_memory();
    let available_memory_bytes = system.available_memory();

    HardwareProfile {
        os_name: System::name().unwrap_or_else(|| std::env::consts::OS.to_string()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".to_string()),
        kernel_version: System::kernel_version(),
        is_apple_silicon: is_apple_silicon(std::env::consts::OS, &cpu_architecture),
        cpu_model,
        cpu_architecture,
        logical_cpu_count: system.cpus().len() as u32,
        physical_core_count: System::physical_core_count().map(|n| n as u32),
        total_memory_bytes,
        available_memory_bytes,
        memory_used_fraction: used_fraction(total_memory_bytes, available_memory_bytes),
        cpu_usage_percent,
        memory_pressure: memory_pressure(),
    }
}

fn is_apple_silicon(os: &str, architecture: &str) -> bool {
    os == "macos" && matches!(architecture, "arm64" | "aarch64")
}

fn used_fraction(total: u64, available: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (1.0 - available.min(total) as f64 / total as f64) as f32
}

/// Maps the macOS `kern.memorystatus_vm_pressure_level` value.
fn pressure_from_level(level: u32) -> Option<MemoryPressure> {
    match level {
        1 => Some(MemoryPressure::Normal),
        2 => Some(MemoryPressure::Warning),
        4 => Some(MemoryPressure::Critical),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn memory_pressure() -> Option<MemoryPressure> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.memorystatus_vm_pressure_level"])
        .output()
        .ok()?;
    let level = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    pressure_from_level(level)
}

#[cfg(not(target_os = "macos"))]
fn memory_pressure() -> Option<MemoryPressure> {
    None
}

impl HardwareProfile {
    /// One line for the log; contains nothing personal.
    pub fn log_line(&self) -> String {
        const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
        format!(
            "{} {} ({}), {}, {} logical CPUs, {:.1} GiB RAM ({:.1} GiB available), memory pressure: {}",
            self.os_name,
            self.os_version,
            self.cpu_architecture,
            self.cpu_model,
            self.logical_cpu_count,
            self.total_memory_bytes as f64 / GIB,
            self.available_memory_bytes as f64 / GIB,
            match self.memory_pressure {
                Some(MemoryPressure::Normal) => "normal",
                Some(MemoryPressure::Warning) => "warning",
                Some(MemoryPressure::Critical) => "critical",
                None => "unknown",
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_plausible_profile() {
        let profile = detect(false);
        assert!(profile.logical_cpu_count >= 1);
        assert!(profile.total_memory_bytes > 256 * 1024 * 1024);
        assert!(profile.available_memory_bytes <= profile.total_memory_bytes);
        assert!((0.0..=1.0).contains(&profile.memory_used_fraction));
        assert!(!profile.cpu_architecture.is_empty());
        assert!(!profile.os_name.is_empty());
        assert_eq!(profile.cpu_usage_percent, None);
        if let Some(physical) = profile.physical_core_count {
            assert!(physical >= 1 && physical <= profile.logical_cpu_count);
        }
        assert!(profile.log_line().contains("logical CPUs"));
    }

    #[test]
    fn samples_cpu_usage_on_request() {
        let usage = detect(true).cpu_usage_percent.expect("usage was requested");
        assert!((0.0..=100.0).contains(&usage), "usage was {usage}");
    }

    #[test]
    fn apple_silicon_requires_macos_and_arm() {
        assert!(is_apple_silicon("macos", "arm64"));
        assert!(is_apple_silicon("macos", "aarch64"));
        assert!(!is_apple_silicon("macos", "x86_64"));
        assert!(!is_apple_silicon("linux", "aarch64"));
    }

    #[test]
    fn memory_fraction_is_bounded() {
        assert_eq!(used_fraction(0, 0), 0.0);
        assert_eq!(used_fraction(8, 8), 0.0);
        assert_eq!(used_fraction(8, 2), 0.75);
        // "Available" larger than total can be reported by some kernels.
        assert_eq!(used_fraction(8, 16), 0.0);
    }

    #[test]
    fn maps_macos_pressure_levels() {
        assert_eq!(pressure_from_level(1), Some(MemoryPressure::Normal));
        assert_eq!(pressure_from_level(2), Some(MemoryPressure::Warning));
        assert_eq!(pressure_from_level(4), Some(MemoryPressure::Critical));
        assert_eq!(pressure_from_level(3), None);
    }
}
