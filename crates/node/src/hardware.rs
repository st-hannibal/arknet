//! Hardware probe — operator visibility into what the binary sees.
//!
//! Best-effort across Linux / macOS / Windows. `sysinfo` handles CPU +
//! RAM + disk uniformly; GPU detection is platform-specific and falls
//! back to "unknown" when toolchain-specific probes aren't available.
//!
//! This report is intentionally text-only and informational; no
//! decisions in Phase 0 depend on its output.
//!
//! `probe` is called from `arknet start` at boot (Day 5). Suppressed
//! during the Day 1 scaffold.

#![allow(dead_code)]

use std::fmt;

use serde::{Deserialize, Serialize};
use sysinfo::{Disks, System};

/// A snapshot of what the host looks like at startup.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareReport {
    /// OS name as reported by `sysinfo` (e.g. `macOS`, `Linux`, `Windows`).
    pub os: String,
    /// OS version / build string.
    pub os_version: String,
    /// Architecture reported by the compiler (`aarch64`, `x86_64`, ...).
    pub arch: &'static str,
    /// Number of logical CPUs.
    pub logical_cpus: usize,
    /// Total RAM in bytes.
    pub total_ram_bytes: u64,
    /// Available RAM in bytes at probe time.
    pub available_ram_bytes: u64,
    /// Free disk on the data directory's filesystem, best effort.
    pub free_disk_bytes: Option<u64>,
    /// GPU detection — free-form text. `"none detected"` when probe
    /// fails or finds nothing.
    pub gpu: String,
}

impl HardwareReport {
    /// Probe the host. Cheap — no heavy sampling.
    pub fn probe() -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_all();

        Self {
            os: System::name().unwrap_or_else(|| "unknown".into()),
            os_version: System::long_os_version().unwrap_or_else(|| "unknown".into()),
            arch: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            total_ram_bytes: sys.total_memory(),
            available_ram_bytes: sys.available_memory(),
            free_disk_bytes: first_disk_free(),
            gpu: probe_gpu(),
        }
    }
}

fn first_disk_free() -> Option<u64> {
    let disks = Disks::new_with_refreshed_list();
    disks.list().iter().map(|d| d.available_space()).max()
}

/// Platform-specific GPU probe. Best-effort: no dependencies on
/// NVML / ROCm / Metal SDKs. Phase 0's goal is "is there a GPU at
/// all?" visibility, not enumeration or capability detection.
#[cfg(target_os = "macos")]
fn probe_gpu() -> String {
    // `system_profiler SPDisplaysDataType` is the canonical macOS probe.
    // We parse the first `Chipset Model:` line we see.
    use std::process::Command;

    let out = Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .output();
    let Ok(out) = out else {
        return "none detected".into();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Chipset Model:") {
            return rest.trim().to_string();
        }
    }
    "none detected".into()
}

#[cfg(target_os = "linux")]
fn probe_gpu() -> String {
    // Two cheap signals: NVIDIA driver file + `/sys/class/drm/` entries.
    if std::path::Path::new("/proc/driver/nvidia/version").exists() {
        if let Ok(s) = std::fs::read_to_string("/proc/driver/nvidia/version") {
            if let Some(first) = s.lines().next() {
                return first.to_string();
            }
        }
        return "NVIDIA (version unknown)".into();
    }

    // Fall back to a DRM device count — works for Intel / AMD integrated
    // cards and covers the "is there anything" case.
    let drm_present = std::fs::read_dir("/sys/class/drm")
        .map(|it| {
            it.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("card"))
        })
        .unwrap_or(false);
    if drm_present {
        "DRM GPU present (vendor unknown)".into()
    } else {
        "none detected".into()
    }
}

#[cfg(target_os = "windows")]
fn probe_gpu() -> String {
    // PowerShell query against Win32_VideoController — the standard
    // WMI probe. Captures vendor + name in one call.
    use std::process::Command;

    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_VideoController | Select-Object -First 1 -ExpandProperty Name",
        ])
        .output();
    let Ok(out) = out else {
        return "none detected".into();
    };
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        "none detected".into()
    } else {
        s
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn probe_gpu() -> String {
    "unsupported platform".into()
}

impl fmt::Display for HardwareReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "  OS:           {} {} ({})",
            self.os, self.os_version, self.arch
        )?;
        writeln!(f, "  CPU:          {} logical cores", self.logical_cpus)?;
        writeln!(
            f,
            "  RAM:          {} / {} GiB available",
            self.available_ram_bytes / (1024 * 1024 * 1024),
            self.total_ram_bytes / (1024 * 1024 * 1024),
        )?;
        match self.free_disk_bytes {
            Some(b) => writeln!(f, "  Disk:         {} GiB free", b / (1024 * 1024 * 1024))?,
            None => writeln!(f, "  Disk:         unknown")?,
        }
        writeln!(f, "  GPU:          {}", self.gpu)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_sane_values() {
        let r = HardwareReport::probe();
        assert!(r.logical_cpus >= 1);
        assert!(r.total_ram_bytes > 0);
        assert!(!r.arch.is_empty());
    }
}
