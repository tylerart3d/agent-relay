//! Auto-routing admission hints, independent of the operator's availability switch.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuActivity {
    pub busy: bool,
    pub external_percent: Option<f32>,
    pub sampled_at_ms: u64,
}

impl GpuActivity {
    pub fn blocks_auto(&self, now_ms: u64) -> bool {
        self.busy && now_ms.saturating_sub(self.sampled_at_ms) <= 90_000
    }
}

/// Ordinary desktop apps (browsers, slicers, virtual cameras, webviews) hold
/// 20-40% of an engine; games and outside CUDA jobs hold far more.
#[cfg(any(windows, test))]
const BUSY_AVERAGE_PERCENT: f32 = 60.0;
#[cfg(any(windows, test))]
const QUIET_AVERAGE_PERCENT: f32 = 40.0;
/// Samples arrive every 15 s, so four samples cover about one minute.
#[cfg(any(windows, test))]
const WINDOW_SAMPLES: usize = 4;
#[cfg(any(windows, test))]
const QUIET_HOLD_MS: u64 = 60_000;

#[cfg(any(windows, test))]
#[derive(Default)]
struct Detector {
    window: std::collections::VecDeque<f32>,
    quiet_since: Option<u64>,
    status: GpuActivity,
}

#[cfg(any(windows, test))]
impl Detector {
    fn update(&mut self, percent: Option<f32>, now: u64) -> GpuActivity {
        let Some(percent) = percent.filter(|v| v.is_finite() && *v >= 0.0) else {
            self.window.clear();
            self.quiet_since = None;
            if !self.status.blocks_auto(now) {
                self.status.busy = false;
            }
            return self.status.clone();
        };
        self.window.push_back(percent.min(100.0));
        while self.window.len() > WINDOW_SAMPLES {
            self.window.pop_front();
        }
        let average = self.window.iter().sum::<f32>() / self.window.len() as f32;
        if self.window.len() == WINDOW_SAMPLES && average >= BUSY_AVERAGE_PERCENT {
            self.quiet_since = None;
            self.status.busy = true;
        } else if average < QUIET_AVERAGE_PERCENT {
            let since = *self.quiet_since.get_or_insert(now);
            if now.saturating_sub(since) >= QUIET_HOLD_MS {
                self.status.busy = false;
            }
        } else {
            self.quiet_since = None;
        }
        self.status.external_percent = Some(percent.min(100.0));
        self.status.sampled_at_ms = now;
        self.status.clone()
    }
}

#[cfg(any(windows, test))]
#[derive(Deserialize)]
struct Process {
    pid: u32,
    parent: u32,
}

#[cfg(any(windows, test))]
#[derive(Deserialize)]
struct Engine {
    instance: String,
    percent: f32,
}

#[cfg(any(windows, test))]
#[derive(Deserialize)]
struct Sample {
    processes: Vec<Process>,
    engines: Vec<Engine>,
}

#[cfg(any(windows, test))]
fn external_percent(sample: Sample, roots: &[u32]) -> Option<f32> {
    use std::collections::{HashMap, HashSet};
    let mut owned: HashSet<u32> = roots.iter().copied().filter(|p| *p > 0).collect();
    // Resolve ancestry, not process names: another Python/CUDA app is outside Relay.
    loop {
        let before = owned.len();
        for p in &sample.processes {
            if owned.contains(&p.parent) {
                owned.insert(p.pid);
            }
        }
        if before == owned.len() {
            break;
        }
    }
    let mut engines = HashMap::<String, f32>::new();
    for e in sample.engines {
        let (pid, engine) = e.instance.strip_prefix("pid_")?.split_once('_')?;
        let pid = pid.parse::<u32>().ok()?;
        if !e.percent.is_finite() || e.percent < 0.0 {
            return None;
        }
        if !owned.contains(&pid) {
            *engines.entry(engine.to_owned()).or_default() += e.percent;
        }
    }
    Some(engines.into_values().fold(0.0, f32::max).min(100.0))
}

#[cfg(windows)]
const COUNTERS: &str = r#"
$ErrorActionPreference='Stop'
$c=Get-Counter '\GPU Engine(*)\Utilization Percentage' -SampleInterval 1 -MaxSamples 1 -ErrorAction SilentlyContinue
$engines=@($c.CounterSamples | Where-Object { $_.Status -eq 0 } | ForEach-Object {
 @{instance=$_.InstanceName;percent=$_.CookedValue}
})
if ($engines.Count -eq 0) { throw 'No valid GPU counters available' }
$processes=@(Get-CimInstance Win32_Process | ForEach-Object {
 @{pid=$_.ProcessId;parent=$_.ParentProcessId}
})
@{engines=$engines;processes=$processes} | ConvertTo-Json -Compress -Depth 4
"#;

pub async fn monitor(fleet: crate::fleet::SharedFleetService) {
    #[cfg(windows)]
    {
        use std::time::Duration;
        let mut detector = Detector::default();
        loop {
            let snapshot = fleet.snapshot();
            let local = snapshot
                .hosts
                .iter()
                .find(|h| h.id == snapshot.local_host_id);
            let supervisor = local.and_then(|h| h.llama_swap.pid).filter(|p| *p > 0);
            let mut command = tokio::process::Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command", COUNTERS]);
            command.creation_flags(0x0800_0000).kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(8), command.output()).await;
            let percent = match output {
                Ok(Ok(output)) if output.status.success() => {
                    serde_json::from_slice::<Sample>(&output.stdout)
                        .ok()
                        .and_then(|sample| {
                            // If the supervisor cannot be identified, don't mistake an
                            // adopted model process for an outside workload.
                            if local.is_some_and(|h| h.loaded_model_id.is_some())
                                && supervisor.is_none()
                            {
                                None
                            } else {
                                external_percent(
                                    sample,
                                    &[std::process::id(), supervisor.unwrap_or(0)],
                                )
                            }
                        })
                }
                _ => None,
            };
            fleet.update_local_gpu_activity(detector.update(percent, crate::fleet::now_ms()));
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }
    #[cfg(not(windows))]
    let _ = fleet; // No unsupported macOS GPU-utilization claims.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_load_and_cooldown_prevent_flapping() {
        let mut d = Detector::default();
        assert!(!d.update(Some(85.0), 0).busy);
        assert!(!d.update(Some(85.0), 15_000).busy);
        assert!(!d.update(Some(85.0), 30_000).busy);
        assert!(d.update(Some(85.0), 45_000).busy);
        // One quiet sample does not lower a one-minute average below the quiet line.
        assert!(d.update(Some(0.0), 60_000).busy);
        assert!(d.update(Some(0.0), 75_000).busy); // average 42.5
        assert!(d.update(Some(0.0), 90_000).busy); // quiet since 90 s
        assert!(d.update(Some(0.0), 135_000).busy);
        assert!(!d.update(Some(0.0), 150_000).busy);
    }

    #[test]
    fn ordinary_desktop_apps_never_block_auto() {
        let mut d = Detector::default();
        for (i, percent) in [24.0, 35.0, 31.0, 28.0, 38.5, 33.0, 26.0, 35.0]
            .into_iter()
            .enumerate()
        {
            assert!(!d.update(Some(percent), i as u64 * 15_000).busy);
        }
    }

    #[test]
    fn desktop_noise_after_a_game_still_clears() {
        let mut d = Detector::default();
        for i in 0..4 {
            d.update(Some(95.0), i * 15_000);
        }
        assert!(d.status.busy);
        // 24-35% background load must not keep the host blocked indefinitely.
        let mut busy = true;
        for i in 4..16 {
            busy = d.update(Some(30.0), i * 15_000).busy;
        }
        assert!(!busy);
    }

    #[test]
    fn failed_samples_do_not_count_as_quiet_or_block_forever() {
        let mut d = Detector::default();
        for i in 0..4 {
            d.update(Some(80.0), i * 15_000);
        }
        assert!(d.update(None, 60_000).busy);
        assert!(d.update(Some(0.0), 90_000).busy);
        assert!(d.update(None, 100_000).busy);
        assert!(!d.update(None, 181_000).busy);
    }

    #[test]
    fn excludes_owned_descendants_but_counts_other_python_processes() {
        let sample: Sample = serde_json::from_value(serde_json::json!({
            "processes":[{"pid":30,"parent":20},{"pid":20,"parent":10},{"pid":40,"parent":1}],
            "engines":[
                {"instance":"pid_30_luid_a_phys_0_eng_0_engtype_3d","percent":80.0},
                {"instance":"pid_40_luid_a_phys_0_eng_0_engtype_3d","percent":35.0},
                {"instance":"pid_41_luid_a_phys_0_eng_0_engtype_3d","percent":10.0},
                {"instance":"pid_40_luid_a_phys_0_eng_1_engtype_copy","percent":20.0}
            ]
        }))
        .unwrap();
        assert_eq!(external_percent(sample, &[10]), Some(45.0));
    }
}
