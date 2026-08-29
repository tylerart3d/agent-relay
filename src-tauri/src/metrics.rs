use std::time::{Duration, Instant};

#[cfg(windows)]
use std::process::Command;

use sysinfo::System;

use crate::fleet::SharedFleetService;
use crate::telemetry::SharedTelemetry;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const IDLE_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
const HISTORY_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(any(windows, test))]
const MEBIBYTE: u64 = 1024 * 1024;

struct MemorySample {
    used_bytes: u64,
    total_bytes: u64,
    kind: String,
}

pub async fn monitor(fleet: SharedFleetService, telemetry: SharedTelemetry) {
    let mut system = System::new();
    let mut last_history_sample = Instant::now() - HISTORY_INTERVAL;
    loop {
        system.refresh_memory();
        let sample = nvidia_memory().unwrap_or_else(|| MemorySample {
            used_bytes: system.used_memory(),
            total_bytes: system.total_memory(),
            kind: if cfg!(target_os = "macos") {
                "Unified memory".into()
            } else {
                "System memory".into()
            },
        });
        fleet.update_local_memory(sample.used_bytes, sample.total_bytes, sample.kind);
        if last_history_sample.elapsed() >= HISTORY_INTERVAL {
            telemetry.record_host_snapshot(&fleet.snapshot());
            last_history_sample = Instant::now();
        }
        tokio::time::sleep(sample_interval(fleet.local_runtime_active())).await;
    }
}

fn sample_interval(runtime_active: bool) -> Duration {
    if runtime_active {
        SAMPLE_INTERVAL
    } else {
        IDLE_SAMPLE_INTERVAL
    }
}

#[cfg(windows)]
fn nvidia_memory() -> Option<MemorySample> {
    use std::os::windows::process::CommandExt;

    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_nvidia_memory(&String::from_utf8_lossy(&output.stdout)))?
}

#[cfg(not(windows))]
fn nvidia_memory() -> Option<MemorySample> {
    None
}

#[cfg(any(windows, test))]
fn parse_nvidia_memory(output: &str) -> Option<MemorySample> {
    let mut fields = output.lines().next()?.split(',').map(str::trim);
    let name = fields.next()?;
    let used_mib = fields.next()?.parse::<u64>().ok()?;
    let total_mib = fields.next()?.parse::<u64>().ok()?;
    (total_mib > 0).then(|| MemorySample {
        used_bytes: used_mib.saturating_mul(MEBIBYTE),
        total_bytes: total_mib.saturating_mul(MEBIBYTE),
        kind: format!(
            "{} VRAM",
            name.strip_prefix("NVIDIA GeForce ").unwrap_or(name)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_csv_memory_values() {
        let sample = parse_nvidia_memory("NVIDIA GeForce RTX 4080, 14671, 16376\n")
            .expect("parse NVIDIA sample");
        assert_eq!(sample.used_bytes, 14_671 * MEBIBYTE);
        assert_eq!(sample.total_bytes, 16_376 * MEBIBYTE);
        assert_eq!(sample.kind, "RTX 4080 VRAM");
    }

    #[test]
    fn rejects_incomplete_nvidia_output() {
        assert!(parse_nvidia_memory("NVIDIA GeForce RTX 4080, N/A\n").is_none());
    }

    #[test]
    fn samples_idle_hardware_less_often() {
        assert_eq!(sample_interval(true), Duration::from_secs(5));
        assert_eq!(sample_interval(false), Duration::from_secs(30));
    }
}
