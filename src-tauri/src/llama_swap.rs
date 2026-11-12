use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock, Weak,
    },
    time::{Duration, Instant},
};

use crate::{
    config::{self, LlamaSwapConfig},
    domain::{
        ControlOutcome, ControlState, LlamaSwapState, LlamaSwapStatus, ModelProfile,
        ProfileCapability, WorkloadKind,
    },
    fleet::SharedFleetService,
    telemetry::{now_ms, LifecycleTelemetry, SharedTelemetry},
};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::Value;

const LLAMA_SWAP_VERSION: &str = "v250";
const CANCEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const CANCEL_ALL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONFIG: &str = "# Agent Relay llama-swap profiles\n# No model is loaded at startup. Add model profiles under `models`.\nhealthCheckTimeout: 600\nglobalTTL: 1800\nunloadTimeout: 5\nmodels: {}\n";

pub(crate) struct VoiceSessionLease {
    supervisor: Weak<LlamaSwapSupervisor>,
    id: String,
    pub cancelled: tokio::sync::watch::Receiver<bool>,
}

impl Drop for VoiceSessionLease {
    fn drop(&mut self) {
        if let Some(supervisor) = self.supervisor.upgrade() {
            supervisor
                .voice_sessions
                .lock()
                .expect("voice sessions poisoned")
                .remove(&self.id);
            supervisor
                .fleet
                .update_active_requests(supervisor.active_count());
        }
    }
}

pub struct LlamaSwapSupervisor {
    endpoint: String,
    config_dir: PathBuf,
    config_path: PathBuf,
    client: reqwest::Client,
    control_client: reqwest::Client,
    child: Mutex<Option<Child>>,
    status: RwLock<LlamaSwapStatus>,
    fleet: SharedFleetService,
    inflight: RwLock<Vec<InflightRequest>>,
    voice_sessions: Mutex<BTreeMap<String, tokio::sync::watch::Sender<bool>>>,
    generation: AtomicU64,
    adopted: AtomicBool,
    /// Shared so a detached cold start can own the lock after its requester leaves.
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    availability_update: Mutex<()>,
    pub(crate) admissions: Arc<crate::admission::Admissions>,
    background_wait: Mutex<BackgroundWait>,
    telemetry: SharedTelemetry,
}

#[cfg(not(test))]
const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_secs(90);
#[cfg(test)]
const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// How long refused background work may wait before an idle foreground model
/// is released. Without this, a client that polls more often than the profile
/// TTL keeps the foreground model resident and starves background work.
#[cfg(not(test))]
const FOREGROUND_RELEASE_AFTER: Duration = Duration::from_secs(180);
#[cfg(test)]
const FOREGROUND_RELEASE_AFTER: Duration = Duration::from_millis(300);
/// Background demand with no refusal for this long is treated as gone.
const BACKGROUND_DEMAND_STALE: Duration = Duration::from_secs(120);

const HOST_OCCUPIED: &str =
    "host_occupied: foreground model is resident; background work must wait for its unload";

#[derive(Default)]
struct BackgroundWait {
    since: Option<Instant>,
    last_refusal: Option<Instant>,
}

impl BackgroundWait {
    /// Record a refusal and report whether background work has waited long enough.
    fn refused(&mut self, now: Instant) -> bool {
        let stale = self
            .last_refusal
            .is_none_or(|t| now.saturating_duration_since(t) > BACKGROUND_DEMAND_STALE);
        if stale || self.since.is_none() {
            self.since = Some(now);
        }
        self.last_refusal = Some(now);
        self.since
            .is_some_and(|t| now.saturating_duration_since(t) >= FOREGROUND_RELEASE_AFTER)
    }
}

impl LlamaSwapSupervisor {
    pub fn new(
        config: &LlamaSwapConfig,
        config_dir: &Path,
        fleet: SharedFleetService,
        telemetry: SharedTelemetry,
    ) -> Result<Self, String> {
        let config_path = resolve_config_path(config_dir, &config.config_path);
        ensure_default_config(&config_path)?;
        let endpoint = format!("http://{}", config.listen_address);
        let status = LlamaSwapStatus {
            state: LlamaSwapState::Stopped,
            version: LLAMA_SWAP_VERSION.into(),
            endpoint: endpoint.clone(),
            config_path: config_path.display().to_string(),
            pid: None,
            error: None,
        };

        Ok(Self {
            endpoint,
            config_dir: config_dir.to_path_buf(),
            config_path,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(2))
                .build()
                .map_err(|error| format!("failed to create llama-swap client: {error}"))?,
            control_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .map_err(|error| format!("failed to create llama-swap control client: {error}"))?,
            child: Mutex::new(None),
            status: RwLock::new(status),
            admissions: fleet.admissions.clone(),
            fleet,
            inflight: RwLock::new(Vec::new()),
            voice_sessions: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            adopted: AtomicBool::new(false),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            availability_update: Mutex::new(()),
            background_wait: Mutex::new(BackgroundWait::default()),
            telemetry,
        })
    }

    pub fn status(&self) -> LlamaSwapStatus {
        self.status
            .read()
            .expect("llama-swap status poisoned")
            .clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn reapply_context_overrides(&self) -> Result<bool, String> {
        let local_host_id = self.fleet.snapshot().local_host_id;
        reapply_model_context_overrides(
            &self.config_path,
            &local_host_id,
            &config::get_model_context_overrides(&self.config_dir)?,
        )
    }

    pub fn start(self: &Arc<Self>) -> Result<LlamaSwapStatus, String> {
        self.start_inner()
    }

    fn start_inner(self: &Arc<Self>) -> Result<LlamaSwapStatus, String> {
        let mut child_slot = self.child.lock().expect("llama-swap child poisoned");
        if child_slot.is_some() {
            return Ok(self.status());
        }

        if endpoint_is_listening(&self.endpoint) {
            let pid = match inspect_adoptable_endpoint(&self.endpoint) {
                Ok(pid) => pid,
                Err(error) => {
                    self.set_status(LlamaSwapState::Error, None, Some(error.clone()));
                    return Err(error);
                }
            };
            self.adopted.store(true, Ordering::SeqCst);
            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.set_status(LlamaSwapState::Starting, pid, None);
            drop(child_slot);

            let weak = Arc::downgrade(self);
            tauri::async_runtime::spawn(async move {
                monitor(weak).await;
            });
            let weak = Arc::downgrade(self);
            tauri::async_runtime::spawn(async move {
                monitor_inflight(weak, generation).await;
            });
            return Ok(self.status());
        }

        self.set_status(LlamaSwapState::Starting, None, None);
        self.adopted.store(false, Ordering::SeqCst);
        let config_path = self
            .config_path
            .to_str()
            .ok_or_else(|| "llama-swap config path is not valid UTF-8".to_owned())?;
        let listen_address = self
            .endpoint
            .strip_prefix("http://")
            .unwrap_or(&self.endpoint);
        let executable = match sidecar_executable_path() {
            Ok(executable) => executable,
            Err(error) => {
                self.set_status(LlamaSwapState::Error, None, Some(error.clone()));
                return Err(error);
            }
        };
        let child = match spawn_service_process(&executable, config_path, listen_address) {
            Ok(spawned) => spawned,
            Err(error) => {
                let error = format!("failed to start llama-swap: {error}");
                self.set_status(LlamaSwapState::Error, None, Some(error.clone()));
                return Err(error);
            }
        };
        let pid = child.id();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *child_slot = Some(child);
        drop(child_slot);
        self.set_status(LlamaSwapState::Starting, Some(pid), None);

        let weak = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            monitor(weak).await;
        });
        let weak = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            monitor_inflight(weak, generation).await;
        });
        Ok(self.status())
    }

    pub async fn load_model(
        self: &Arc<Self>,
        model_id: &str,
        force: bool,
    ) -> Result<ControlOutcome, String> {
        self.load_model_with_context(model_id, force, None).await
    }

    pub(crate) async fn set_available(&self, available: bool) -> Result<(), String> {
        // Publish immediately: already admitted work may finish, but queued admissions
        // must see Off when they obtain the lifecycle lock. No runtime is cancelled.
        let _update = self
            .availability_update
            .lock()
            .map_err(|_| "Availability update lock poisoned")?;
        config::set_available(&self.config_dir, available)?;
        self.fleet.set_local_available(available);
        Ok(())
    }

    pub(crate) fn track_inference(
        &self,
        model: &str,
    ) -> Result<crate::admission::SlotLease, String> {
        let models = self.background_models()?;
        self.admissions
            .track_checked(model, models.iter().any(|m| m == model), !models.is_empty())
    }

    fn background_models(&self) -> Result<Vec<String>, String> {
        Ok(self.local_host_status()?.reclaim.models)
    }

    async fn check_background_residency(&self, model: &str) -> Result<(), String> {
        let backgrounds = self.background_models()?;
        if !backgrounds.iter().any(|m| m == model) {
            return Ok(());
        }
        self.admissions.check_background(true)?;
        let host = self.local_host_status()?;
        let running = self
            .control_client
            .get(format!("{}/running", self.endpoint))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json::<RunningResponse>()
            .await
            .map_err(|e| e.to_string())?;
        // None marks a running entry without a model ID: it blocks and is never released.
        let blockers: Vec<Option<&str>> = running
            .running
            .iter()
            .map(|r| r.get("model").and_then(Value::as_str))
            .filter(|id| {
                id.is_none_or(|id| {
                    id != model
                        && !backgrounds.iter().any(|m| m == id)
                        && !host
                            .models
                            .iter()
                            .any(|m| m.id == id && m.is_independent_service())
                })
            })
            .collect();
        if blockers.is_empty() {
            *self
                .background_wait
                .lock()
                .expect("background wait poisoned") = BackgroundWait::default();
            return Ok(());
        }
        let waited = self
            .background_wait
            .lock()
            .expect("background wait poisoned")
            .refused(Instant::now());
        if waited {
            if let Some(ids) = blockers.into_iter().collect::<Option<Vec<&str>>>() {
                // Best effort: on any doubt the foreground model stays resident.
                if let Err(error) = self.release_idle_foreground(&host, &ids).await {
                    eprintln!("idle foreground release skipped: {error}");
                }
            }
        }
        // This request still waits; its retry after Retry-After finds the host free.
        Err(HOST_OCCUPIED.into())
    }

    /// Unload resident foreground models only when nothing is using them. Called
    /// with the lifecycle lock held, so no new foreground request can start here.
    /// Never cancels a request; a busy model is left alone.
    async fn release_idle_foreground(
        &self,
        host: &crate::domain::HostStatus,
        ids: &[&str],
    ) -> Result<bool, String> {
        if !self
            .voice_sessions
            .lock()
            .expect("voice sessions poisoned")
            .is_empty()
        {
            return Ok(false);
        }
        for id in ids {
            let Some(profile) = host.models.iter().find(|m| m.id == *id) else {
                return Ok(false);
            };
            if profile.kind != WorkloadKind::Text
                || self.admissions.model_or_reclaim_busy(id)
                || self
                    .inflight
                    .read()
                    .expect("inflight poisoned")
                    .iter()
                    .any(|r| r.model == *id)
            {
                return Ok(false);
            }
            // llama.cpp exposes native slots; also catch work that bypassed Relay.
            if profile.runtime == "llama.cpp"
                && self.model_capacity(id).await?["busy_slots"].as_u64() != Some(0)
            {
                return Ok(false);
            }
        }
        let started = Instant::now();
        for id in ids {
            let result = self.unload_one(id).await;
            // Record each release so an unexpected unload can be explained later.
            self.telemetry.record_lifecycle(LifecycleTelemetry {
                occurred_at_ms: now_ms(),
                host_id: self.fleet.local_host_id().to_owned(),
                model_id: Some((*id).to_owned()),
                action: "release_for_background".into(),
                outcome: if result.is_ok() { "applied" } else { "error" }.into(),
                duration_ms: started.elapsed().as_millis() as u64,
                forced: false,
            });
            result?;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let running = self
                    .control_client
                    .get(format!("{}/running", self.endpoint))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .error_for_status()
                    .map_err(|e| e.to_string())?
                    .json::<RunningResponse>()
                    .await
                    .map_err(|e| e.to_string())?;
                if !running.running.iter().any(|r| {
                    r.get("model")
                        .and_then(Value::as_str)
                        .is_some_and(|m| ids.contains(&m))
                }) {
                    return Ok::<_, String>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "foreground model did not unload".to_owned())??;
        *self
            .background_wait
            .lock()
            .expect("background wait poisoned") = BackgroundWait::default();
        self.sync_inventory().await?;
        Ok(true)
    }

    async fn cancel_reclaimed_model(&self, model: &str) -> Result<(), String> {
        let _guard = self.lifecycle.lock().await;
        self.fleet.require_local_available()?;
        if !self.background_models()?.iter().any(|m| m == model) {
            return Err("Background policy changed; cancellation abandoned".into());
        }
        if self
            .local_host_status()?
            .gpu_activity
            .as_ref()
            .is_some_and(|g| g.blocks_auto(crate::fleet::now_ms()))
        {
            return Err("Auto declined: GPU is busy with another application".into());
        }
        self.admissions.cancel_model(model);
        let requests = self
            .inflight
            .read()
            .expect("inflight poisoned")
            .iter()
            .filter(|r| r.model == model)
            .cloned()
            .collect();
        cancel_inflight_requests(
            &self.control_client,
            &self.endpoint,
            requests,
            CANCEL_REQUEST_TIMEOUT,
            CANCEL_ALL_TIMEOUT,
        )
        .await;
        // A profile-specific unload also stops native-only requests; never unload all or embeddings.
        self.unload_one(model).await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let running = self
                    .control_client
                    .get(format!("{}/running", self.endpoint))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .error_for_status()
                    .map_err(|e| e.to_string())?
                    .json::<RunningResponse>()
                    .await
                    .map_err(|e| e.to_string())?;
                if !running.running.iter().any(|r| r["model"] == model) {
                    return Ok::<_, String>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "Background model did not unload; interactive load abandoned".to_owned())??;
        self.inflight
            .write()
            .expect("inflight poisoned")
            .retain(|r| r.model != model);
        self.sync_inventory().await
    }

    /// Hold the lifecycle lock until the caller registers its active request.
    /// Auto may only replace explicitly reclaimable background profiles after draining.
    pub(crate) async fn prepare_inference(
        self: &Arc<Self>,
        model_id: &str,
        path: &str,
        auto: bool,
    ) -> Result<(String, tokio::sync::OwnedMutexGuard<()>), String> {
        let guard = self.lifecycle.clone().lock_owned().await;
        self.fleet.require_local_available()?;
        if auto
            && self
                .local_host_status()?
                .gpu_activity
                .as_ref()
                .is_some_and(|g| g.blocks_auto(crate::fleet::now_ms()))
        {
            return Err("Auto declined: GPU is busy with another application".into());
        }
        self.check_background_residency(model_id).await?;
        if let Some(endpoint) = self.ready_model_endpoint(model_id, path).await? {
            return Ok((endpoint, guard));
        }
        if auto {
            self.sync_inventory().await?;
            let host = self.local_host_status()?;
            // /v1/models can still say "unloaded" while another process starts.
            // Consult /running as well, so a timed-out cold start is not evicted.
            let running = self
                .control_client
                .get(format!("{}/running", self.endpoint))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .json::<RunningResponse>()
                .await
                .map_err(|e| e.to_string())?;
            let cpu_embedding = |id: &str| {
                host.models
                    .iter()
                    .any(|m| m.id == id && m.is_independent_service())
            };
            let background = self.background_models()?;
            let may_reclaim = !background.iter().any(|id| id == model_id)
                && self
                    .admissions
                    .snapshot(background.clone())
                    .target
                    .as_deref()
                    == Some(model_id);
            let replaceable =
                |id: &str| cpu_embedding(id) || (may_reclaim && background.iter().any(|m| m == id));
            // Fail closed if native slot activity cannot be verified. Check again under
            // the lifecycle lock immediately before any background model is unloaded.
            for r in &running.running {
                if let Some(id) = r
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|id| background.iter().any(|m| m == id))
                {
                    if may_reclaim
                        && (self.admissions.background_count(&background) != 0
                            || self.model_capacity(id).await?["busy_slots"].as_u64() != Some(0))
                    {
                        return Err("capacity_busy: background requests are still finishing".into());
                    }
                }
            }
            let occupied = host
                .loaded_model_id
                .as_deref()
                .is_some_and(|id| id != model_id && !replaceable(id))
                || running.running.iter().any(|r| {
                    r.get("model")
                        .and_then(Value::as_str)
                        .is_none_or(|id| id != model_id && !replaceable(id))
                });
            let background_requests = self
                .inflight
                .read()
                .expect("inflight poisoned")
                .iter()
                .filter(|r| cpu_embedding(&r.model))
                .count() as u32;
            if occupied || host.active_requests.saturating_sub(background_requests) != 0 {
                return Err("Auto cold start declined: this machine is no longer idle".into());
            }
        }
        // Run the cold start in its own task that owns the lifecycle lock. If the
        // requesting client gives up (OpenCode's provider timeout is 5 minutes),
        // dropping this future no longer aborts a slow start halfway: the model
        // finishes loading for the client's retry, and idle TTL unloads it if
        // nobody comes back. llama-swap's health-check timeout still bounds it.
        let this = self.clone();
        let model = model_id.to_owned();
        let path = path.to_owned();
        tokio::spawn(async move {
            let outcome = this.load_model_inner(&model, false, None, false).await?;
            if outcome.state == ControlState::Conflict {
                return Err(outcome.message);
            }
            let endpoint = this
                .ready_model_endpoint(&model, &path)
                .await?
                .ok_or_else(|| format!("{model} did not become ready"))?;
            Ok((endpoint, guard))
        })
        .await
        .map_err(|error| format!("model load task failed: {error}"))?
    }

    pub(crate) async fn prepare_auto(
        self: &Arc<Self>,
        model_id: &str,
        path: &str,
    ) -> Result<(), String> {
        tokio::time::timeout(
            crate::fleet::PEER_LOAD_TIMEOUT,
            self.prepare_inference(model_id, path, true),
        )
        .await
        .map_err(|_| "Auto model preparation timed out".to_owned())?
        .map(|_| ())
    }

    /// A host-owned, single-use claim closes the gap between readiness and inference.
    pub(crate) async fn reserve_auto(self: &Arc<Self>, model_id: &str) -> Result<String, String> {
        self.fleet.require_local_available()?;
        let host = self.local_host_status()?;
        if host
            .gpu_activity
            .as_ref()
            .is_some_and(|g| g.blocks_auto(crate::fleet::now_ms()))
        {
            return Err("Auto declined: GPU is busy with another application".into());
        }
        let profile = host
            .models
            .iter()
            .find(|p| p.id == model_id)
            .ok_or("Unknown model")?;
        let background = self.background_models()?;
        let is_background = background.iter().any(|m| m == model_id);
        let displaced = host
            .loaded_model_id
            .as_ref()
            .filter(|id| *id != model_id && background.contains(id));
        let _reclaim = if let Some(displaced) = displaced.filter(|_| !is_background) {
            let guard = self.admissions.begin_reclaim(model_id, &background)?;
            let drained = tokio::time::timeout(BACKGROUND_DRAIN_TIMEOUT, async {
                loop {
                    self.fleet.require_local_available()?;
                    if self
                        .local_host_status()?
                        .gpu_activity
                        .as_ref()
                        .is_some_and(|g| g.blocks_auto(crate::fleet::now_ms()))
                    {
                        return Err(
                            "Auto declined: GPU is busy with another application".to_owned()
                        );
                    }
                    if !self.background_models()?.contains(displaced) {
                        return Err(
                            "Background reclaim policy changed; prompt not submitted".into()
                        );
                    }
                    let capacity = self.model_capacity(displaced).await?;
                    // unloaded is safe; unsupported/unavailable activity is not.
                    let idle =
                        capacity["busy_slots"].as_u64() == Some(0) || capacity["loaded"] == false;
                    if self.admissions.background_count(&background) == 0 && idle {
                        return Ok::<_, String>(());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
            .await;
            match drained {
                Ok(result) => result?,
                Err(_) => self.cancel_reclaimed_model(displaced).await?,
            }
            Some(guard)
        } else {
            None
        };
        let mut slots = profile.parallel_slots.unwrap_or(1).clamp(1, 256);
        let mut busy = self
            .inflight
            .read()
            .expect("inflight poisoned")
            .iter()
            .filter(|r| r.model == model_id)
            .count() as u32;
        if host.loaded_model_id.as_deref() == Some(model_id) {
            if let Ok(capacity) = self.model_capacity(model_id).await {
                if let Some(actual) = capacity["total_slots"].as_u64() {
                    slots = slots.min(actual.clamp(1, 256) as u32);
                }
                busy = busy.max(capacity["busy_slots"].as_u64().unwrap_or(0) as u32);
            }
        }
        let lease = self.admissions.reserve_classified(
            model_id,
            slots,
            busy,
            is_background,
            !background.is_empty(),
        )?;
        if _reclaim.is_some() {
            tokio::time::timeout(Duration::from_secs(120), self.prepare_auto(model_id, "v1/chat/completions"))
                .await.map_err(|_| "capacity_busy: interactive model preparation timed out; retry after it becomes ready".to_owned())??;
        } else {
            self.prepare_auto(model_id, "v1/chat/completions").await?;
        }
        Ok(lease.into_ticket())
    }

    pub async fn load_model_with_context(
        self: &Arc<Self>,
        model_id: &str,
        force: bool,
        context_window: Option<u32>,
    ) -> Result<ControlOutcome, String> {
        let started = Instant::now();
        let _lifecycle_guard = self.lifecycle.lock().await;
        let result = self
            .load_model_inner(model_id, force, context_window, false)
            .await;
        self.record_lifecycle("load", Some(model_id), force, started, &result);
        result
    }

    pub async fn reload_model(
        self: &Arc<Self>,
        model_id: &str,
        force: bool,
        context_window: Option<u32>,
    ) -> Result<ControlOutcome, String> {
        let started = Instant::now();
        let _guard = self.lifecycle.lock().await;
        self.sync_inventory().await?;
        let local = self.local_host_status()?;
        if !local
            .models
            .iter()
            .any(|p| p.id == model_id && p.is_independent_service())
            && local
                .loaded_model_id
                .as_deref()
                .is_some_and(|id| id != model_id)
        {
            return Err("The host is serving a different model; reroute before reloading".into());
        }
        let result = self
            .load_model_inner(model_id, force, context_window, true)
            .await;
        self.record_lifecycle("reload", Some(model_id), force, started, &result);
        result
    }

    pub async fn ready_model_endpoint(
        &self,
        model_id: &str,
        path_and_query: &str,
    ) -> Result<Option<String>, String> {
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            self.control_client
                .get(format!("{}/running", self.endpoint))
                .send(),
        )
        .await
        .map_err(|_| "llama-swap running-model lookup timed out".to_owned())?
        .map_err(|error| format!("failed to inspect llama-swap running models: {error}"))?
        .error_for_status()
        .map_err(|error| format!("llama-swap running-model lookup failed: {error}"))?
        .json::<RunningResponse>()
        .await
        .map_err(|error| format!("invalid llama-swap running-model response: {error}"))?;

        Self::ready_tracked_endpoint(&self.endpoint, &response.running, model_id, path_and_query)
    }

    /// Read-only local capacity: never load a model or refresh its idle timer.
    /// Only monitoring uses the direct port; inference still traverses swap.
    pub async fn model_capacity(&self, model_id: &str) -> Result<Value, String> {
        let local = self.local_host_status()?;
        let profile = local
            .models
            .iter()
            .find(|p| p.id == model_id)
            .ok_or("unknown local model")?;
        if profile.runtime != "llama.cpp" {
            return Err("slot inspection is supported only for llama.cpp profiles".into());
        }
        let operation = async {
            let running = self
                .control_client
                .get(format!("{}/running", self.endpoint))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .json::<RunningResponse>()
                .await
                .map_err(|e| e.to_string())?;
            let Some(model) = running
                .running
                .iter()
                .find(|m| m["model"] == model_id && m["state"] == "ready")
            else {
                return Ok(serde_json::json!({"model": model_id, "loaded": false,
                    "total_slots": null, "busy_slots": null, "slots": []}));
            };
            let proxy = model
                .get("proxy")
                .and_then(Value::as_str)
                .ok_or("missing runtime URL")?;
            let response = self
                .control_client
                .get(format!("{}/slots", proxy.trim_end_matches('/')))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| e.to_string())?;
                if bytes.len() + chunk.len() > 262_144 {
                    return Err("slot response too large".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            summarize_slots(
                model_id,
                &serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
            )
        };
        tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .map_err(|_| "capacity lookup timed out".to_owned())?
    }

    fn ready_tracked_endpoint(
        endpoint: &str,
        running_models: &[Value],
        model_id: &str,
        path_and_query: &str,
    ) -> Result<Option<String>, String> {
        let ready = running_models.iter().any(|running| {
            let is_ready = running.get("model").and_then(Value::as_str) == Some(model_id)
                && running.get("state").and_then(Value::as_str) == Some("ready");
            is_ready && running.get("proxy").and_then(Value::as_str).is_some()
        });
        if !ready {
            return Ok(None);
        }

        // Inference must traverse llama-swap's normal model-dispatch route
        // rather than the raw `proxy` URL or its /upstream passthrough. The
        // dispatched process ServeHTTP call owns both the in-flight guard and
        // the last-use timestamp used by idle TTL expiry. Agent Relay has
        // already rewritten the JSON model field to this local profile ID.
        Ok(Some(format!(
            "{}/{}",
            endpoint.trim_end_matches('/'),
            path_and_query.trim_start_matches('/')
        )))
    }

    pub async fn ready_upstream_endpoint(
        &self,
        model_id: &str,
        path_and_query: &str,
    ) -> Result<Option<String>, String> {
        let ready = self.ready_model_endpoint(model_id, path_and_query).await?;
        Ok(ready.map(|_| {
            format!(
                "{}/upstream/{}/{}",
                self.endpoint.trim_end_matches('/'),
                urlencoding::encode(model_id),
                path_and_query.trim_start_matches('/')
            )
        }))
    }

    /// Register under the lifecycle lock so unload/swap cannot slip between
    /// readiness and session registration. llama-swap's upstream WebSocket
    /// keeps its process TTL alive; this lease provides Relay's busy/cancel UI.
    pub(crate) async fn begin_voice_session(
        self: &Arc<Self>,
        model_id: &str,
    ) -> Result<(String, VoiceSessionLease), String> {
        let _lock = self.lifecycle.lock().await;
        self.sync_inventory().await?;
        let profile = self
            .local_host_status()?
            .models
            .into_iter()
            .find(|profile| profile.id == model_id && profile.is_voice_service())
            .ok_or("Unknown voice profile")?;
        let outcome = self
            .load_model_inner(&profile.id, false, None, false)
            .await?;
        if outcome.state == ControlState::Conflict {
            return Err(outcome.message);
        }
        let endpoint = self
            .ready_upstream_endpoint(model_id, "v1/realtime")
            .await?
            .ok_or("Voice runtime did not become ready")?;
        let id = uuid::Uuid::new_v4().to_string();
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        self.voice_sessions
            .lock()
            .expect("voice sessions poisoned")
            .insert(id.clone(), cancel);
        self.fleet.update_active_requests(self.active_count());
        Ok((
            endpoint,
            VoiceSessionLease {
                supervisor: Arc::downgrade(self),
                id,
                cancelled,
            },
        ))
    }

    async fn load_model_inner(
        self: &Arc<Self>,
        model_id: &str,
        force: bool,
        context_window: Option<u32>,
        reload: bool,
    ) -> Result<ControlOutcome, String> {
        self.fleet.require_local_available()?;
        let local = self.local_host_status()?;
        let profile = local.models.iter().find(|model| model.id == model_id);
        let Some(profile) = profile else {
            return Err(format!("unknown local model profile: {model_id}"));
        };
        self.check_background_residency(model_id).await?;
        if let Some(context_window) = context_window {
            config::validate_client_context_window(context_window)?;
        }
        // Profiles that advertise a context length have a fixed launch-time
        // context that Agent Relay can manage. Runtimes such as MLX and MTPLX
        // omit it because they negotiate or bound context themselves.
        let qualified_model = format!("{}/{model_id}", local.id);
        let persisted_context = config::get_model_context_overrides(&self.config_dir)?
            .get(&qualified_model)
            .copied();
        let managed_context =
            managed_context_request(profile.context_length, context_window.or(persisted_context));
        let context_matches =
            managed_context.is_none_or(|requested| profile.context_length == Some(requested));
        // Validate runtime-specific configuration before saving an override or
        // unloading a working model. A rejected change must leave both intact.
        let context_rewrite = managed_context
            .filter(|_| !context_matches)
            .map(|requested| prepare_model_context(&self.config_path, model_id, requested))
            .transpose()?;
        // Independent embeddings have their own non-exclusive, persistent llama-swap
        // group. Never unload/cancel chat or vision to start or reload them.
        if profile.is_independent_service() {
            if self
                .inflight
                .read()
                .expect("inflight poisoned")
                .iter()
                .any(|r| r.model == model_id)
                && !force
            {
                return Ok(self.outcome(
                    ControlState::Conflict,
                    "embedding requests are active".into(),
                ));
            }
            if reload || !context_matches {
                self.unload_one(model_id).await?;
            }
            if let Some(plan) = context_rewrite {
                plan.apply(&self.config_path)?;
                self.wait_for_profile_context(model_id, managed_context.unwrap())
                    .await?;
            }
            if let Some(context) = context_window {
                config::set_model_context_override(&self.config_dir, qualified_model, context)?;
            }
            self.control_client
                .get(model_probe_url(&self.endpoint, model_id))
                .send()
                .await
                .map_err(|e| format!("failed to load embeddings: {e}"))?
                .error_for_status()
                .map_err(|e| format!("failed to load embeddings: {e}"))?;
            self.sync_inventory().await?;
            return Ok(self.outcome(
                ControlState::Applied,
                format!("loaded independent embeddings {model_id}"),
            ));
        }
        if !reload && local.loaded_model_id.as_deref() == Some(model_id) && context_matches {
            if let Some(context) = managed_context_request(profile.context_length, context_window) {
                config::set_model_context_override(&self.config_dir, qualified_model, context)?;
            }
            return Ok(self.outcome(ControlState::Noop, format!("{model_id} is already loaded")));
        }

        // Worker profiles (supervised HTTP services) do not compete with the text
        // model for the single active slot: loading one never evicts the other, and
        // in-flight text requests do not block a worker load. llama-swap groups
        // decide what actually stays resident.
        let loaded_is_worker = local
            .loaded_model_id
            .as_deref()
            .and_then(|id| local.models.iter().find(|model| model.id == id))
            .is_some_and(ModelProfile::is_worker_service);
        let worker_involved = profile.is_worker_service() || loaded_is_worker;

        let embedding_requests = self
            .inflight
            .read()
            .expect("inflight poisoned")
            .iter()
            .filter(|r| {
                local
                    .models
                    .iter()
                    .any(|p| p.id == r.model && p.is_independent_service())
            })
            .count();
        let active = self
            .active_count()
            .saturating_sub(u32::try_from(embedding_requests).unwrap_or(u32::MAX));
        if active > 0 && !force && !worker_involved {
            return Ok(self.outcome(
                ControlState::Conflict,
                format!("{active} request(s) are currently using this host"),
            ));
        }
        if let Some(context) = managed_context_request(profile.context_length, context_window) {
            config::set_model_context_override(&self.config_dir, qualified_model, context)?;
        }
        if force {
            self.cancel_all_inflight().await;
            self.unload_upstream().await?;
        } else if local.loaded_model_id.is_some() && !worker_involved {
            self.unload_foreground_models(&local.models).await?;
        }

        if let Some(context_window) = managed_context.filter(|_| !context_matches) {
            context_rewrite
                .expect("context rewrite was validated")
                .apply(&self.config_path)?;
            if self
                .wait_for_profile_context(model_id, context_window)
                .await
                .is_err()
            {
                // Older adopted llama-swap processes may predate
                // --watch-config. The model is already unloaded, so restart
                // only the lightweight control service and retry discovery.
                self.stop_inner().await?;
                self.start_inner()?;
                self.wait_for_profile_context(model_id, context_window)
                    .await?;
            }
        }

        self.control_client
            .get(if profile.is_voice_service() {
                format!(
                    "{}/upstream/{}/v1/realtime/health",
                    self.endpoint,
                    urlencoding::encode(model_id)
                )
            } else {
                model_probe_url(&self.endpoint, model_id)
            })
            .send()
            .await
            .map_err(|error| format!("failed to load {model_id}: {error}"))?
            .error_for_status()
            .map_err(|error| format!("llama-swap failed to load {model_id}: {error}"))?;
        self.sync_inventory().await?;
        Ok(self.outcome(ControlState::Applied, format!("loaded {model_id}")))
    }

    async fn wait_for_profile_context(
        &self,
        model_id: &str,
        context_window: u32,
    ) -> Result<(), String> {
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let models = self
                .client
                .get(format!("{}/v1/models", self.endpoint))
                .send()
                .await;
            let Ok(models) = models else { continue };
            let Ok(models) = models.json::<ModelsResponse>().await else {
                continue;
            };
            if models.data.iter().any(|model| {
                model.id == model_id && model_context_length(&model.meta) == Some(context_window)
            }) {
                return Ok(());
            }
        }
        Err(format!(
            "llama-swap did not reload {model_id} with a {context_window}-token context"
        ))
    }

    pub async fn unload_models(&self, force: bool) -> Result<ControlOutcome, String> {
        let started = Instant::now();
        let model_id = self
            .local_host_status()
            .ok()
            .and_then(|host| host.loaded_model_id);
        let _lifecycle_guard = self.lifecycle.lock().await;
        let result = self.unload_models_inner(force).await;
        self.record_lifecycle("unload", model_id.as_deref(), force, started, &result);
        result
    }

    async fn unload_models_inner(&self, force: bool) -> Result<ControlOutcome, String> {
        let active = self.active_count();
        if active > 0 && !force {
            return Ok(self.outcome(
                ControlState::Conflict,
                format!("{active} request(s) are currently using this host"),
            ));
        }
        if force {
            self.cancel_all_inflight().await;
        }
        if self.local_host_status()?.loaded_model_id.is_none() {
            return Ok(self.outcome(ControlState::Noop, "host is already idle".into()));
        }

        self.unload_upstream().await?;
        self.sync_inventory().await?;
        Ok(self.outcome(ControlState::Applied, "unloaded local model".into()))
    }

    pub async fn stop_service(&self, force: bool) -> Result<ControlOutcome, String> {
        let started = Instant::now();
        let model_id = self
            .local_host_status()
            .ok()
            .and_then(|host| host.loaded_model_id);
        let _lifecycle_guard = self.lifecycle.lock().await;
        let active = self.active_count();
        if active > 0 && !force {
            let result = Ok(self.outcome(
                ControlState::Conflict,
                format!("{active} request(s) are currently using this host"),
            ));
            self.record_lifecycle("stop", model_id.as_deref(), force, started, &result);
            return result;
        }
        let result = self.stop_inner().await.map(|(_, applied)| {
            self.outcome(
                if applied {
                    ControlState::Applied
                } else {
                    ControlState::Noop
                },
                if applied {
                    "stopped local llama-swap"
                } else {
                    "local llama-swap is already stopped"
                }
                .into(),
            )
        });
        self.record_lifecycle("stop", model_id.as_deref(), force, started, &result);
        result
    }

    async fn stop_inner(&self) -> Result<(LlamaSwapStatus, bool), String> {
        let has_child = self
            .child
            .lock()
            .expect("llama-swap child poisoned")
            .is_some();
        let adopted = self.adopted.load(Ordering::SeqCst);
        let endpoint_was_listening = endpoint_is_listening(&self.endpoint);
        if !has_child && !adopted && !endpoint_was_listening {
            self.mark_runtime_stopped();
            return Ok((self.status(), false));
        }

        let adopted_pid = if (adopted || !has_child) && endpoint_was_listening {
            Some(verified_adopted_listener_pid(
                &self.endpoint,
                self.status().pid,
            )?)
        } else {
            None
        };

        self.generation.fetch_add(1, Ordering::SeqCst);
        self.cancel_all_inflight().await;
        if endpoint_was_listening {
            let _ = self.unload_upstream().await;
        }
        let child = self.child.lock().expect("llama-swap child poisoned").take();
        if let Some(mut child) = child {
            let pid = child.id();
            if let Err(error) = child.kill() {
                self.adopted.store(true, Ordering::SeqCst);
                let error = format!("failed to stop llama-swap: {error}");
                self.set_status(LlamaSwapState::Error, Some(pid), Some(error.clone()));
                return Err(error);
            }
            let _ = child.wait();
        } else if let Some(pid) = adopted_pid {
            if let Err(error) = terminate_adopted_process(pid) {
                // The verified adopted process can exit between listener discovery
                // and taskkill/kill. If its endpoint is already gone, the desired
                // stopped state was reached and the stale PID error is harmless.
                if endpoint_is_listening(&self.endpoint) {
                    self.set_status(LlamaSwapState::Error, Some(pid), Some(error.clone()));
                    return Err(error);
                }
            }
        }
        if !wait_for_endpoint_to_close(&self.endpoint, Duration::from_secs(5)).await {
            let error = format!(
                "llama-swap endpoint {} is still listening after stop",
                self.endpoint
            );
            self.set_status(LlamaSwapState::Error, None, Some(error.clone()));
            return Err(error);
        }
        self.adopted.store(false, Ordering::SeqCst);
        self.mark_runtime_stopped();
        Ok((self.status(), true))
    }

    pub async fn restart_service(self: &Arc<Self>, force: bool) -> Result<ControlOutcome, String> {
        let started = Instant::now();
        let model_id = self
            .local_host_status()
            .ok()
            .and_then(|host| host.loaded_model_id);
        let _lifecycle_guard = self.lifecycle.lock().await;
        let active = self.active_count();
        if active > 0 && !force {
            let result = Ok(self.outcome(
                ControlState::Conflict,
                format!("{active} request(s) are currently using this host"),
            ));
            self.record_lifecycle("restart", model_id.as_deref(), force, started, &result);
            return result;
        }
        let result = async {
            self.stop_inner().await?;
            self.start_inner()?;
            Ok(self.outcome(ControlState::Applied, "restarted local llama-swap".into()))
        }
        .await;
        self.record_lifecycle("restart", model_id.as_deref(), force, started, &result);
        result
    }

    fn record_lifecycle(
        &self,
        action: &str,
        model_id: Option<&str>,
        forced: bool,
        started: Instant,
        result: &Result<ControlOutcome, String>,
    ) {
        let outcome = match result {
            Ok(outcome) => match outcome.state {
                ControlState::Applied => "applied",
                ControlState::Noop => "noop",
                ControlState::Conflict => "conflict",
            },
            Err(_) => "error",
        };
        self.telemetry.record_lifecycle(LifecycleTelemetry {
            occurred_at_ms: now_ms(),
            host_id: self.fleet.local_host_id().to_owned(),
            model_id: model_id.map(str::to_owned),
            action: action.to_owned(),
            outcome: outcome.to_owned(),
            duration_ms: started.elapsed().as_millis() as u64,
            forced,
        });
    }

    pub(crate) async fn sync_inventory(&self) -> Result<(), String> {
        let models = self
            .client
            .get(format!("{}/v1/models", self.endpoint))
            .send()
            .await
            .map_err(|error| format!("failed to list llama-swap models: {error}"))?
            .error_for_status()
            .map_err(|error| format!("llama-swap model listing failed: {error}"))?
            .json::<ModelsResponse>()
            .await
            .map_err(|error| format!("invalid llama-swap model listing: {error}"))?;

        let loaded_model_id = loaded_model_id(&models.data);
        let profiles = models
            .data
            .into_iter()
            .map(|model| ModelProfile {
                priority: if model_metadata(&model.meta)
                    .and_then(|m| m.get("priority"))
                    .and_then(Value::as_str)
                    == Some("background")
                {
                    crate::domain::ProfilePriority::Background
                } else {
                    crate::domain::ProfilePriority::Foreground
                },
                display_name: model
                    .name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| model.id.clone()),
                runtime: model_runtime(&model.meta),
                kind: model_kind(&model.meta),
                capabilities: model_capabilities(&model.meta),
                lifecycle_adapter: "llama_swap".into(),
                resource_pool: model_resource_pool(&model.meta),
                context_length: model_context_length(&model.meta),
                parallel_slots: model_metadata(&model.meta)
                    .and_then(|m| m.get("context_slots"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| (1..=256).contains(n)),
                inference_controls: model_inference_controls(&model.meta),
                id: model.id,
            })
            .collect();
        self.set_status(LlamaSwapState::Ready, self.status().pid, None);
        self.fleet.update_local_runtime(
            self.status(),
            profiles,
            loaded_model_id,
            self.active_count(),
        );
        Ok(())
    }

    async fn unload_one(&self, model_id: &str) -> Result<(), String> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.control_client
                .post(format!(
                    "{}/api/models/unload/{}",
                    self.endpoint,
                    urlencoding::encode(model_id)
                ))
                .send(),
        )
        .await
        .map_err(|_| "model unload timed out".to_owned())?
        .map_err(|e| format!("model unload failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("model unload failed: {e}"))?;
        Ok(())
    }

    async fn unload_foreground_models(&self, profiles: &[ModelProfile]) -> Result<(), String> {
        if !profiles.iter().any(ModelProfile::is_independent_service) {
            return self.unload_upstream().await;
        }
        let running = self
            .control_client
            .get(format!("{}/running", self.endpoint))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json::<RunningResponse>()
            .await
            .map_err(|e| e.to_string())?;
        for entry in running.running {
            let id = entry
                .get("model")
                .and_then(Value::as_str)
                .ok_or("running model entry has no model ID")?;
            if !profiles
                .iter()
                .any(|p| p.id == id && p.is_independent_service())
            {
                self.unload_one(id).await?;
            }
        }
        Ok(())
    }

    async fn unload_upstream(&self) -> Result<(), String> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.control_client
                .post(format!("{}/api/models/unload", self.endpoint))
                .send(),
        )
        .await
        .map_err(|_| "llama-swap unload timed out".to_owned())?
        .map_err(|error| format!("failed to unload llama-swap models: {error}"))?
        .error_for_status()
        .map_err(|error| format!("llama-swap unload failed: {error}"))?;
        self.verify_unloaded().await
    }

    async fn verify_unloaded(&self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(response) = self
                    .client
                    .get(format!("{}/running", self.endpoint))
                    .send()
                    .await
                {
                    if let Ok(running) = response.json::<RunningResponse>().await {
                        if running.running.is_empty() {
                            return Ok(());
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "llama-swap still reports a running model after unload".to_owned())?
    }

    async fn cancel_all_inflight(&self) {
        for cancel in self
            .voice_sessions
            .lock()
            .expect("voice sessions poisoned")
            .values()
        {
            cancel.send_replace(true);
        }
        let requests = self
            .inflight
            .read()
            .expect("inflight requests poisoned")
            .clone();
        cancel_inflight_requests(
            &self.control_client,
            &self.endpoint,
            requests,
            CANCEL_REQUEST_TIMEOUT,
            CANCEL_ALL_TIMEOUT,
        )
        .await;
        self.inflight
            .write()
            .expect("inflight requests poisoned")
            .clear();
        self.fleet.update_active_requests(self.active_count());
    }

    fn active_count(&self) -> u32 {
        self.inflight
            .read()
            .expect("inflight requests poisoned")
            .len() as u32
            + self
                .voice_sessions
                .lock()
                .expect("voice sessions poisoned")
                .len() as u32
    }

    fn local_host_status(&self) -> Result<crate::domain::HostStatus, String> {
        let snapshot = self.fleet.snapshot();
        snapshot
            .hosts
            .into_iter()
            .find(|host| host.id == snapshot.local_host_id)
            .ok_or_else(|| "local host is missing from fleet configuration".to_owned())
    }

    fn outcome(&self, state: ControlState, message: String) -> ControlOutcome {
        let local = self.local_host_status().ok();
        ControlOutcome {
            state,
            host_id: self.fleet.local_host_id().to_owned(),
            active_requests: self.active_count(),
            loaded_model_id: local.and_then(|host| host.loaded_model_id),
            message,
        }
    }

    fn apply_event(&self, data: &str) {
        let Ok(envelope) = serde_json::from_str::<EventEnvelope>(data) else {
            return;
        };
        if envelope.kind != "inflight" {
            return;
        }
        let Ok(update) = serde_json::from_str::<InflightUpdate>(&envelope.data) else {
            return;
        };
        let mut inflight = self.inflight.write().expect("inflight requests poisoned");
        apply_inflight_update(&mut inflight, update);
        drop(inflight);
        self.fleet.update_active_requests(self.active_count());
    }

    fn handle_process_error(&self, generation: u64, pid: u32, error: String) {
        let child_pid = self
            .child
            .lock()
            .expect("llama-swap child poisoned")
            .as_ref()
            .map(Child::id);
        if !child_event_matches(
            self.generation.load(Ordering::SeqCst),
            child_pid,
            generation,
            pid,
        ) {
            return;
        }
        self.set_status(LlamaSwapState::Error, Some(pid), Some(error));
    }

    fn handle_terminated(&self, generation: u64, pid: u32, code: Option<i32>) {
        let mut child = self.child.lock().expect("llama-swap child poisoned");
        let child_pid = child.as_ref().map(Child::id);
        if !child_event_matches(
            self.generation.load(Ordering::SeqCst),
            child_pid,
            generation,
            pid,
        ) {
            return;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.inflight
            .write()
            .expect("inflight requests poisoned")
            .clear();
        self.fleet.update_active_requests(0);
        child.take();
        drop(child);
        self.adopted.store(false, Ordering::SeqCst);
        let current = self.status();
        if current.state != LlamaSwapState::Stopped {
            self.set_status(
                LlamaSwapState::Error,
                None,
                Some(format!("llama-swap exited unexpectedly with code {code:?}")),
            );
            self.clear_runtime_model();
        }
    }

    fn mark_runtime_stopped(&self) {
        self.set_status(LlamaSwapState::Stopped, None, None);
        self.clear_runtime_model();
    }

    fn clear_runtime_model(&self) {
        let profiles = self
            .local_host_status()
            .map(|host| host.models)
            .unwrap_or_default();
        self.fleet
            .update_local_runtime(self.status(), profiles, None, 0);
    }

    fn set_status(&self, state: LlamaSwapState, pid: Option<u32>, error: Option<String>) {
        let mut status = self.status.write().expect("llama-swap status poisoned");
        status.state = state;
        status.pid = pid;
        status.error = error;
        let updated = status.clone();
        drop(status);
        self.fleet.update_llama_swap_status(updated);
    }
}

async fn cancel_inflight_requests(
    client: &reqwest::Client,
    endpoint: &str,
    requests: Vec<InflightRequest>,
    request_timeout: Duration,
    overall_timeout: Duration,
) {
    let cancellations = stream::iter(requests).for_each_concurrent(Some(8), |request| {
        let encoded = urlencoding::encode(&request.id);
        let url = format!("{endpoint}/api/inflight/{encoded}/cancel");
        async move {
            let _ = tokio::time::timeout(request_timeout, client.post(url).send()).await;
        }
    });
    let _ = tokio::time::timeout(overall_timeout, cancellations).await;
}

pub type SharedLlamaSwapSupervisor = Arc<LlamaSwapSupervisor>;

fn child_event_matches(
    current_generation: u64,
    current_pid: Option<u32>,
    event_generation: u64,
    event_pid: u32,
) -> bool {
    current_generation == event_generation && current_pid == Some(event_pid)
}

fn sidecar_path_from_executable(executable: &Path) -> Result<PathBuf, String> {
    let directory = executable.parent().ok_or_else(|| {
        format!(
            "Agent Relay executable has no parent directory: {}",
            executable.display()
        )
    })?;
    let name = if cfg!(windows) {
        "llama-swap.exe"
    } else {
        "llama-swap"
    };
    Ok(directory.join(name))
}

fn sidecar_executable_path() -> Result<PathBuf, String> {
    let current = env::current_exe()
        .map_err(|error| format!("failed to locate the Agent Relay executable: {error}"))?;
    let sidecar = sidecar_path_from_executable(&current)?;
    if sidecar.is_file() {
        Ok(sidecar)
    } else {
        Err(format!(
            "bundled llama-swap executable is missing: {}",
            sidecar.display()
        ))
    }
}

fn spawn_service_process(
    executable: &Path,
    config_path: &str,
    listen_address: &str,
) -> std::io::Result<Child> {
    let mut command = Command::new(executable);
    command
        .args([
            "--config",
            config_path,
            "--listen",
            listen_address,
            "--watch-config",
        ])
        .stdin(Stdio::null());

    configure_independent_process(&mut command);
    command.spawn()
}

fn configure_independent_process(command: &mut Command) {
    command.stdout(Stdio::null()).stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
    }
}

async fn monitor(supervisor: Weak<LlamaSwapSupervisor>) {
    let mut startup_attempts = 0;
    loop {
        let Some(supervisor) = supervisor.upgrade() else {
            return;
        };
        let child_state = {
            let mut child = supervisor.child.lock().expect("llama-swap child poisoned");
            child.as_mut().map(|child| (child.id(), child.try_wait()))
        };
        let has_child = child_state.is_some();
        if !has_child && !supervisor.adopted.load(Ordering::SeqCst) {
            return;
        }

        match child_state {
            Some((pid, Ok(Some(status)))) => {
                supervisor.handle_terminated(
                    supervisor.generation.load(Ordering::SeqCst),
                    pid,
                    status.code(),
                );
                return;
            }
            Some((pid, Err(error))) => {
                supervisor.handle_process_error(
                    supervisor.generation.load(Ordering::SeqCst),
                    pid,
                    error.to_string(),
                );
            }
            _ => {}
        }

        match supervisor.sync_inventory().await {
            Ok(()) => startup_attempts = 0,
            Err(error) => {
                startup_attempts += 1;
                if startup_attempts >= 20 {
                    let pid = supervisor.status().pid;
                    supervisor.set_status(LlamaSwapState::Error, pid, Some(error));
                }
            }
        }
        drop(supervisor);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn endpoint_is_listening(endpoint: &str) -> bool {
    let Some(address) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let Ok(mut addresses) = address.to_socket_addrs() else {
        return false;
    };
    addresses
        .any(|address| TcpStream::connect_timeout(&address, Duration::from_millis(150)).is_ok())
}

fn inspect_adoptable_endpoint(endpoint: &str) -> Result<Option<u32>, String> {
    if !endpoint_is_llama_swap(endpoint) {
        return Err(format!(
            "{} is already in use by a service that is not llama-swap",
            endpoint
        ));
    }

    let discovered = discover_listener_pid(endpoint)?;
    let requested = match env::var("AGENTRELAY_ADOPT_LLAMA_SWAP_PID") {
        Ok(value) => Some(value.parse::<u32>().map_err(|_| {
            "AGENTRELAY_ADOPT_LLAMA_SWAP_PID must contain a valid process ID".to_owned()
        })?),
        Err(env::VarError::NotPresent) => None,
        Err(error) => {
            return Err(format!(
                "failed to read AGENTRELAY_ADOPT_LLAMA_SWAP_PID: {error}"
            ))
        }
    };

    if let Some(requested) = requested {
        if discovered != Some(requested) {
            return Err(format!(
                "refusing llama-swap handoff: process {requested} does not own {endpoint}"
            ));
        }
    }
    let pid = requested.or(discovered);
    if let Some(pid) = pid {
        verify_llama_swap_process(pid)?;
    }
    Ok(pid)
}

fn verified_adopted_listener_pid(endpoint: &str, recorded_pid: Option<u32>) -> Result<u32, String> {
    if !endpoint_is_llama_swap(endpoint) {
        return Err(format!(
            "refusing to stop the unrecognized service listening at {endpoint}"
        ));
    }
    let pid = discover_listener_pid(endpoint)?.ok_or_else(|| {
        format!("cannot determine which process owns the llama-swap endpoint {endpoint}")
    })?;
    if recorded_pid.is_some_and(|recorded| recorded != pid) {
        return Err(format!(
            "refusing to stop process {pid}: Agent Relay adopted process {}",
            recorded_pid.expect("recorded PID was checked")
        ));
    }
    verify_llama_swap_process(pid)?;
    Ok(pid)
}

fn endpoint_is_llama_swap(endpoint: &str) -> bool {
    let Some(address) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let Ok(addresses) = address.to_socket_addrs() else {
        return false;
    };
    addresses.into_iter().any(|address| {
        probe_running_endpoint(address)
            .ok()
            .flatten()
            .is_some_and(|value| value.get("running").is_some_and(Value::is_array))
    })
}

fn probe_running_endpoint(address: std::net::SocketAddr) -> std::io::Result<Option<Value>> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(300))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(300)))?;
    write!(
        stream,
        "GET /running HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;

    let mut response = Vec::new();
    let _ = stream.take(64 * 1024).read_to_end(&mut response);
    Ok(parse_json_http_response(&response))
}

fn parse_json_http_response(response: &[u8]) -> Option<Value> {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&response[..boundary]).ok()?;
    let status = headers.lines().next()?;
    if !status.split_whitespace().nth(1)?.starts_with('2') {
        return None;
    }
    let body = &response[boundary + 4..];
    let decoded;
    let body = if headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    }) {
        decoded = decode_chunked_body(body)?;
        decoded.as_slice()
    } else {
        body
    };
    serde_json::from_slice(body).ok()
}

fn decode_chunked_body(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    loop {
        let line_end = body.windows(2).position(|window| window == b"\r\n")?;
        let size = std::str::from_utf8(&body[..line_end])
            .ok()?
            .split(';')
            .next()
            .and_then(|value| usize::from_str_radix(value.trim(), 16).ok())?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(decoded);
        }
        if body.len() < size + 2 || &body[size..size + 2] != b"\r\n" {
            return None;
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn endpoint_port(endpoint: &str) -> Result<u16, String> {
    let address = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported llama-swap endpoint: {endpoint}"))?;
    address
        .to_socket_addrs()
        .map_err(|error| format!("cannot resolve llama-swap endpoint {endpoint}: {error}"))?
        .next()
        .map(|address| address.port())
        .ok_or_else(|| format!("llama-swap endpoint has no socket address: {endpoint}"))
}

fn discover_listener_pid(endpoint: &str) -> Result<Option<u32>, String> {
    let port = endpoint_port(endpoint)?;
    let listed = platform_listener_pid(port);
    if let Ok(Some(pid)) = listed {
        return Ok(Some(pid));
    }
    // Some Windows machines omit loopback listeners from netstat entirely, which
    // left an adopted llama-swap impossible to restart. Fall back to the process
    // whose own --listen argument is this endpoint; it must be the only match.
    let address = endpoint.strip_prefix("http://").unwrap_or(endpoint);
    match pid_from_listen_argument(&llama_swap_command_lines(), address) {
        Some(pid) => Ok(Some(pid)),
        None => listed,
    }
}

/// (pid, executable name, arguments) for every running process that looks like llama-swap.
fn llama_swap_command_lines() -> Vec<(u32, String, Vec<String>)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy().into_owned();
            looks_like_llama_swap_process(&name).then(|| {
                let args = process
                    .cmd()
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect();
                (pid.as_u32(), name, args)
            })
        })
        .collect()
}

fn pid_from_listen_argument(
    processes: &[(u32, String, Vec<String>)],
    address: &str,
) -> Option<u32> {
    unique_pid(
        processes
            .iter()
            .filter(|(_, name, args)| {
                looks_like_llama_swap_process(name)
                    && args
                        .windows(2)
                        .any(|pair| pair[0] == "--listen" && pair[1] == address)
            })
            .map(|(pid, _, _)| *pid)
            .collect(),
    )
}

#[cfg(windows)]
fn platform_listener_pid(port: u16) -> Result<Option<u32>, String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("netstat.exe")
        .args(["-ano", "-p", "TCP"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("failed to inspect TCP listeners: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "netstat failed while inspecting llama-swap: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(unique_pid(parse_netstat_listener_pids(
        &String::from_utf8_lossy(&output.stdout),
        port,
    )))
}

#[cfg(windows)]
fn parse_netstat_listener_pids(output: &str, port: u16) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() >= 5
                && fields[0].eq_ignore_ascii_case("TCP")
                && fields[3].eq_ignore_ascii_case("LISTENING")
                && socket_field_port(fields[1]) == Some(port))
            .then(|| fields[4].parse::<u32>().ok())
            .flatten()
        })
        .collect()
}

#[cfg(not(windows))]
fn platform_listener_pid(port: u16) -> Result<Option<u32>, String> {
    let mut last_error = None;
    for executable in ["/usr/sbin/lsof", "lsof"] {
        match Command::new(executable)
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .output()
        {
            Ok(output) if output.status.success() => {
                return Ok(unique_pid(parse_pid_lines(&String::from_utf8_lossy(
                    &output.stdout,
                ))));
            }
            Ok(output) => {
                last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(format!(
        "failed to inspect the llama-swap listener{}",
        last_error
            .filter(|error| !error.is_empty())
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    ))
}

#[cfg(not(windows))]
fn parse_pid_lines(output: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

fn unique_pid(mut pids: Vec<u32>) -> Option<u32> {
    pids.sort_unstable();
    pids.dedup();
    (pids.len() == 1).then(|| pids[0])
}

#[cfg(windows)]
fn verify_llama_swap_process(pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("tasklist.exe")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("failed to identify process {pid}: {error}"))?;
    let name = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && looks_like_llama_swap_process(&name) {
        Ok(())
    } else {
        Err(format!(
            "refusing to adopt process {pid}: its executable is not llama-swap"
        ))
    }
}

#[cfg(not(windows))]
fn verify_llama_swap_process(pid: u32) -> Result<(), String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map_err(|error| format!("failed to identify process {pid}: {error}"))?;
    let name = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && looks_like_llama_swap_process(&name) {
        Ok(())
    } else {
        Err(format!(
            "refusing to adopt process {pid}: its executable is not llama-swap"
        ))
    }
}

fn looks_like_llama_swap_process(value: &str) -> bool {
    value
        .split(|character: char| {
            character.is_whitespace()
                || matches!(character, '"' | '\'' | ',' | '/' | '\\' | '[' | ']')
        })
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .any(|name| {
            name == "llama-swap"
                || name == "llama-swap.exe"
                || name
                    .strip_prefix("llama-swap-")
                    .is_some_and(|suffix| !suffix.is_empty())
        })
}

#[cfg(windows)]
fn socket_field_port(value: &str) -> Option<u16> {
    value.rsplit_once(':')?.1.trim_end_matches(']').parse().ok()
}

async fn wait_for_endpoint_to_close(endpoint: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while endpoint_is_listening(endpoint) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

#[cfg(windows)]
fn terminate_adopted_process(pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| format!("failed to stop adopted llama-swap process: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to stop adopted llama-swap process {pid}: taskkill exited with {status}"
        ))
    }
}

#[cfg(not(windows))]
fn terminate_adopted_process(pid: u32) -> Result<(), String> {
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map_err(|error| format!("failed to stop adopted llama-swap process: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to stop adopted llama-swap process {pid}: kill exited with {status}"
        ))
    }
}

async fn monitor_inflight(supervisor: Weak<LlamaSwapSupervisor>, generation: u64) {
    loop {
        let Some(service) = supervisor.upgrade() else {
            return;
        };
        if service.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let endpoint = format!("{}/api/events", service.endpoint);
        drop(service);

        if let Ok(mut response) = reqwest::Client::new().get(endpoint).send().await {
            let mut buffer = String::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                let Some(service) = supervisor.upgrade() else {
                    return;
                };
                if service.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(boundary) = buffer.find("\n\n") {
                    let event = buffer[..boundary].to_owned();
                    buffer.drain(..boundary + 2);
                    for line in event.lines() {
                        if let Some(data) = line.strip_prefix("data:") {
                            service.apply_event(data);
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn resolve_config_path(config_dir: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

fn model_probe_url(endpoint: &str, model_id: &str) -> String {
    let encoded = urlencoding::encode(model_id);
    format!("{endpoint}/upstream/{encoded}/v1/models")
}

fn reapply_model_context_overrides(
    path: &Path,
    local_host_id: &str,
    overrides: &BTreeMap<String, u32>,
) -> Result<bool, String> {
    let prefix = format!("{local_host_id}/");
    let mut changed = false;
    for (qualified_model, context_window) in overrides {
        let Some(model_id) = qualified_model.strip_prefix(&prefix) else {
            continue;
        };
        match rewrite_model_context(path, model_id, *context_window) {
            Ok(profile_changed) => changed |= profile_changed,
            Err(error) if error.contains(" is missing from ") => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(changed)
}

fn rewrite_model_context(path: &Path, model_id: &str, context_window: u32) -> Result<bool, String> {
    prepare_model_context(path, model_id, context_window)?.apply(path)
}

struct ModelContextRewrite {
    original: String,
    updated: String,
    runtime: Option<crate::strata_context::ContextFile>,
}

impl ModelContextRewrite {
    fn apply(self, path: &Path) -> Result<bool, String> {
        // Refuse to overwrite a config changed by the operator during unload.
        if fs::read_to_string(path).map_err(|error| error.to_string())? != self.original {
            return Err("Model profiles changed while applying context; retry the change".into());
        }
        let mut changed = false;
        if let Some(runtime) = self.runtime {
            if fs::read_to_string(&runtime.path).ok().as_deref() != Some(&runtime.contents) {
                fs::create_dir_all(
                    runtime
                        .path
                        .parent()
                        .ok_or("runtime config has no directory")?,
                )
                .map_err(|error| error.to_string())?;
                config::atomic_write_text(&runtime.path, &runtime.contents)?;
                changed = true;
            }
        }
        if self.updated != self.original {
            config::atomic_write_text(path, &self.updated)?;
            crate::config_watch::record_internal_change(path).map_err(|error| {
                format!("failed to track update to {}: {error}", path.display())
            })?;
            changed = true;
        }
        Ok(changed)
    }
}

fn prepare_model_context(
    path: &Path,
    model_id: &str,
    context_window: u32,
) -> Result<ModelContextRewrite, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| yaml_model_key(line) == Some(model_id))
        .ok_or_else(|| {
            format!(
                "model profile {model_id} is missing from {}",
                path.display()
            )
        })?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let indent = line.len() - line.trim_start().len();
            (indent <= 2).then_some(index)
        })
        .unwrap_or(lines.len());

    // Opted-in multi-slot profiles advertise per-request context. llama.cpp's
    // launch context is the total pool across their fixed parallel slots.
    let context_slots = lines[start + 1..end]
        .iter()
        .find_map(|line| line.trim().strip_prefix("context_slots:"))
        .map(|value| {
            value
                .trim()
                .parse::<u32>()
                .map_err(|_| "invalid context_slots")
        })
        .transpose()?
        .unwrap_or(1);
    if context_slots == 0 {
        return Err("context_slots must be positive".into());
    }
    let replacement = context_window
        .checked_mul(context_slots)
        .ok_or("total context exceeds supported range")?
        .to_string();
    let strata = lines[start + 1..end].iter().any(|line| {
        line.trim()
            .strip_prefix("runtime:")
            .is_some_and(|runtime| runtime.trim().trim_matches(['\'', '"']) == "strata")
    });
    let runtime = if strata {
        Some(crate::strata_context::prepare(
            path,
            model_id,
            &mut lines[start + 1..end],
            context_window,
        )?)
    } else {
        None
    };
    let mut command_updated = false;
    let mut metadata_updated = false;
    for line in &mut lines[start + 1..end] {
        for option in if strata {
            &[][..]
        } else {
            &["--ctx-size", "-c", "LLAMA_ARG_CTX_SIZE"][..]
        } {
            if let Some(updated) = replace_cli_number(line, option, &replacement) {
                *line = updated;
                command_updated = true;
                break;
            }
        }
        if line.trim_start().starts_with("context_length:") {
            let indent = &line[..line.len() - line.trim_start().len()];
            *line = format!("{indent}context_length: {context_window}");
            metadata_updated = true;
        }
    }
    if !command_updated && runtime.is_none() {
        return Err(format!(
            "model profile {model_id} has no --ctx-size, -c, or LLAMA_ARG_CTX_SIZE launch argument"
        ));
    }
    if !metadata_updated {
        let metadata = lines[start + 1..end]
            .iter()
            .position(|line| line.trim() == "metadata:")
            .map(|offset| start + 1 + offset)
            .ok_or_else(|| format!("model profile {model_id} has no metadata block"))?;
        lines.insert(
            metadata + 1,
            format!("      context_length: {context_window}"),
        );
    }
    let newline = if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = lines.join(newline);
    if contents.ends_with('\n') {
        updated.push_str(newline);
    }
    Ok(ModelContextRewrite {
        original: contents,
        updated,
        runtime,
    })
}

fn summarize_slots(model_id: &str, value: &Value) -> Result<Value, String> {
    let entries = value.as_array().ok_or("invalid slots response")?;
    if entries.is_empty() {
        return Err("runtime returned no slots".into());
    }
    let mut slots = Vec::new();
    let mut busy = 0;
    for entry in entries {
        let id = entry["id"].as_u64().ok_or("invalid slot ID")?;
        let context = entry["n_ctx"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("invalid slot context")?;
        let processing = entry["is_processing"]
            .as_bool()
            .ok_or("invalid slot activity")?;
        busy += usize::from(processing);
        slots.push(serde_json::json!({"id": id, "context_window": context,
            "busy": processing, "speculative": entry["speculative"].as_bool()}));
    }
    Ok(serde_json::json!({"model": model_id, "loaded": true,
        "total_slots": slots.len(), "busy_slots": busy, "slots": slots}))
}

fn yaml_model_key(line: &str) -> Option<&str> {
    if !line.starts_with("  ") || line.starts_with("    ") {
        return None;
    }
    let key = line.trim().strip_suffix(':')?.trim();
    Some(key.trim_matches(|character| matches!(character, '\'' | '"')))
}

fn replace_cli_number(line: &str, option: &str, replacement: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    let (separator_required, allow_whitespace) = if option == "LLAMA_ARG_CTX_SIZE" {
        (b'=', false)
    } else {
        (b'=', true)
    };
    while let Some(relative) = line[search_from..].find(option) {
        let option_start = search_from + relative;
        let option_end = option_start + option.len();
        let boundary_before = option_start == 0
            || bytes[option_start - 1].is_ascii_whitespace()
            || matches!(bytes[option_start - 1], b'\'' | b'"');
        let boundary_after = bytes.get(option_end).is_some_and(|next| {
            *next == separator_required || (allow_whitespace && next.is_ascii_whitespace())
        });
        if !boundary_before || !boundary_after {
            search_from = option_end;
            continue;
        }

        let mut number_start = option_end;
        while number_start < bytes.len()
            && (bytes[number_start].is_ascii_whitespace() || bytes[number_start] == b'=')
        {
            number_start += 1;
        }
        let mut number_end = number_start;
        while number_end < bytes.len() && bytes[number_end].is_ascii_digit() {
            number_end += 1;
        }
        if number_end == number_start {
            return None;
        }
        return Some(format!(
            "{}{}{}",
            &line[..number_start],
            replacement,
            &line[number_end..]
        ));
    }
    None
}

fn ensure_default_config(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    fs::write(path, DEFAULT_CONFIG)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))
}

fn model_runtime(meta: &Option<Value>) -> String {
    meta.as_ref()
        .and_then(|meta| meta.get("llamaswap"))
        .and_then(|metadata| metadata.get("runtime"))
        .and_then(Value::as_str)
        .unwrap_or("unspecified")
        .to_owned()
}

fn model_metadata(meta: &Option<Value>) -> Option<&Value> {
    meta.as_ref().and_then(|meta| meta.get("llamaswap"))
}

fn model_kind(meta: &Option<Value>) -> WorkloadKind {
    match model_metadata(meta)
        .and_then(|metadata| metadata.get("kind"))
        .and_then(Value::as_str)
    {
        Some("image") => WorkloadKind::Image,
        Some("worker") => WorkloadKind::Worker,
        Some("voice") => WorkloadKind::Voice,
        _ => WorkloadKind::Text,
    }
}

fn model_capabilities(meta: &Option<Value>) -> Vec<ProfileCapability> {
    let configured = model_metadata(meta)
        .and_then(|metadata| metadata.get("capabilities"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|value| match value {
                    "chat" => Some(ProfileCapability::Chat),
                    "completions" => Some(ProfileCapability::Completions),
                    "responses" => Some(ProfileCapability::Responses),
                    "anthropic_messages" => Some(ProfileCapability::AnthropicMessages),
                    "embeddings" => Some(ProfileCapability::Embeddings),
                    "vision_input" => Some(ProfileCapability::VisionInput),
                    "image_generation" => Some(ProfileCapability::ImageGeneration),
                    "workflow_queue" => Some(ProfileCapability::WorkflowQueue),
                    "http_service" => Some(ProfileCapability::HttpService),
                    "realtime_audio" => Some(ProfileCapability::RealtimeAudio),
                    "text_to_speech" => Some(ProfileCapability::TextToSpeech),
                    "speech_to_text" => Some(ProfileCapability::SpeechToText),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !configured.is_empty() {
        return configured;
    }

    let mut capabilities = match model_kind(meta) {
        WorkloadKind::Image => vec![
            ProfileCapability::ImageGeneration,
            ProfileCapability::WorkflowQueue,
        ],
        WorkloadKind::Worker => vec![ProfileCapability::HttpService],
        WorkloadKind::Voice => vec![ProfileCapability::RealtimeAudio],
        WorkloadKind::Unknown => Vec::new(),
        WorkloadKind::Text => {
            let mut capabilities = vec![
                ProfileCapability::Chat,
                ProfileCapability::Completions,
                ProfileCapability::Responses,
            ];
            if model_runtime(meta).to_ascii_lowercase().contains("llama") {
                capabilities.push(ProfileCapability::AnthropicMessages);
            }
            capabilities
        }
    };
    if model_metadata(meta)
        .and_then(|m| m.get("vision"))
        .and_then(Value::as_bool)
        == Some(true)
        && model_kind(meta) == WorkloadKind::Text
    {
        capabilities.push(ProfileCapability::VisionInput);
    }
    capabilities
}

fn model_resource_pool(meta: &Option<Value>) -> String {
    model_metadata(meta)
        .and_then(|metadata| metadata.get("resource_pool"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("default")
        .to_owned()
}

fn model_context_length(meta: &Option<Value>) -> Option<u32> {
    model_metadata(meta)
        .and_then(|metadata| metadata.get("context_length"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn model_inference_controls(meta: &Option<Value>) -> crate::domain::InferenceControls {
    let Some(controls) =
        model_metadata(meta).and_then(|metadata| metadata.get("inference_controls"))
    else {
        return Default::default();
    };
    serde_json::from_value(controls.clone()).unwrap_or_default()
}

fn managed_context_request(
    advertised_context: Option<u32>,
    requested_context: Option<u32>,
) -> Option<u32> {
    requested_context.filter(|_| advertised_context.is_some())
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelRecord>,
}

#[derive(Deserialize)]
struct ModelRecord {
    id: String,
    name: Option<String>,
    meta: Option<Value>,
    status: ModelStatus,
}

#[derive(Deserialize)]
struct ModelStatus {
    value: String,
}

fn loaded_model_id(models: &[ModelRecord]) -> Option<String> {
    models
        .iter()
        .filter(|model| model.status.value == "loaded")
        .min_by_key(|model| {
            matches!(
                model_resource_pool(&model.meta).as_str(),
                "cpu" | "embeddings"
            ) && model_capabilities(&model.meta) == [ProfileCapability::Embeddings]
        })
        .map(|model| model.id.clone())
}

#[derive(Deserialize)]
struct RunningResponse {
    running: Vec<Value>,
}

#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    kind: String,
    data: String,
}

#[derive(Deserialize)]
struct InflightUpdate {
    operation: String,
    #[serde(default)]
    requests: Vec<InflightRequest>,
    request: Option<InflightRequest>,
    id: Option<String>,
}

#[derive(Clone, Deserialize)]
struct InflightRequest {
    id: String,
    #[allow(dead_code)]
    model: String,
}

fn apply_inflight_update(requests: &mut Vec<InflightRequest>, update: InflightUpdate) {
    match update.operation.as_str() {
        "snapshot" => *requests = update.requests,
        "upsert" => {
            if let Some(request) = update.request {
                if let Some(existing) = requests.iter_mut().find(|entry| entry.id == request.id) {
                    *existing = request;
                } else {
                    requests.push(request);
                }
            }
        }
        "remove" => {
            if let Some(id) = update.id {
                requests.retain(|request| request.id != id);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};

    #[test]
    fn resolves_relative_and_absolute_config_paths() {
        let base = Path::new("config-root");
        assert_eq!(
            resolve_config_path(base, "llama-swap.yaml"),
            base.join("llama-swap.yaml")
        );

        let absolute = std::env::temp_dir().join("fleet-profiles.yaml");
        assert_eq!(
            resolve_config_path(base, absolute.to_str().expect("utf-8 path")),
            absolute
        );
    }

    #[test]
    fn default_profiles_expire_after_thirty_minutes() {
        assert!(DEFAULT_CONFIG.contains("globalTTL: 1800"));
        assert!(!DEFAULT_CONFIG.contains("globalTTL: 0"));
    }

    #[test]
    fn capacity_reports_slots_without_prompt_or_sampler_details() {
        let result = summarize_slots(
            "vision",
            &serde_json::json!([
                {"id": 0, "n_ctx": 16384, "is_processing": true, "speculative": true,
                 "params": {"generation_prompt": "private"}, "id_task": 123},
                {"id": 1, "n_ctx": 16384, "is_processing": false}
            ]),
        )
        .unwrap();
        assert_eq!(result["total_slots"], 2);
        assert_eq!(result["busy_slots"], 1);
        assert_eq!(result["slots"][0]["context_window"], 16384);
        assert!(!result.to_string().contains("private"));
        assert!(result["slots"][0].get("params").is_none());
        for invalid in [
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!([{"id": 0, "n_ctx": 0, "is_processing": true}]),
            serde_json::json!([{"id": 0, "n_ctx": 16384}]),
        ] {
            assert!(summarize_slots("vision", &invalid).is_err());
        }
    }

    #[tokio::test]
    async fn cold_start_finishes_after_the_requesting_client_disconnects() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{routing::get, Json, Router};
        let loaded = Arc::new(AtomicBool::new(false));
        let loads = Arc::new(AtomicU64::new(0));
        let app = Router::new()
            .route("/v1/models", get({ let loaded=loaded.clone(); move || { let loaded=loaded.clone(); async move {
                Json(serde_json::json!({"data":[
                    {"id":"qwen","status":{"value":if loaded.load(Ordering::SeqCst) {"loaded"} else {"unloaded"}}}
                ]}))
            }}}))
            .route("/running", get({ let loaded=loaded.clone(); move || { let loaded=loaded.clone(); async move {
                Json(if loaded.load(Ordering::SeqCst) {
                    serde_json::json!({"running":[{"model":"qwen","state":"ready","proxy":"http://127.0.0.1:9999"}]})
                } else { serde_json::json!({"running":[]}) })
            }}}))
            // A slow start, like Strata sizing its expert cache. If the probe were
            // dropped, axum would drop this handler before it marks the model loaded.
            .route("/upstream/qwen/v1/models", get({ let loaded=loaded.clone(); let loads=loads.clone(); move || {
                let loaded=loaded.clone(); let loads=loads.clone(); async move {
                    loads.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    loaded.store(true, Ordering::SeqCst);
                    "ready"
                }
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory =
            std::env::temp_dir().join(format!("relay-detached-load-{}", uuid::Uuid::new_v4()));
        let mut config = FleetConfig::load_or_create(&directory, "WORKSTATION").unwrap();
        config.llama_swap.listen_address = address.to_string();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let telemetry = Arc::new(TelemetryStore::new(&directory).unwrap());
        let supervisor = Arc::new(
            LlamaSwapSupervisor::new(&config.llama_swap, &directory, fleet, telemetry).unwrap(),
        );
        supervisor.sync_inventory().await.unwrap();
        let request = tokio::spawn({
            let s = supervisor.clone();
            async move {
                s.prepare_inference("qwen", "v1/chat/completions", false)
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The client gives up mid-start.
        request.abort();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !loaded.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the abandoned start still completes");
        // The retry finds the model ready, without starting it again, once the
        // detached start releases the lifecycle lock.
        let (endpoint, _guard) = tokio::time::timeout(
            Duration::from_secs(3),
            supervisor.prepare_inference("qwen", "v1/chat/completions", false),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(endpoint.ends_with("/v1/chat/completions"));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn auto_admission_cold_starts_without_eviction_and_availability_survives_restart() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{routing::get, Json, Router};
        let loaded = Arc::new(AtomicBool::new(false));
        let foreign_starting = Arc::new(AtomicBool::new(false));
        let loads = Arc::new(AtomicU64::new(0));
        let forbidden = Arc::new(AtomicU64::new(0));
        let app = Router::new()
            .route("/v1/models", get({ let loaded=loaded.clone(); move || { let loaded=loaded.clone(); async move {
                Json(serde_json::json!({"data":[
                    {"id":"qwen","status":{"value":if loaded.load(Ordering::SeqCst) {"loaded"} else {"unloaded"}}},
                    {"id":"other","status":{"value":"unloaded"}}
                ]}))
            }}}))
            .route("/running", get({ let loaded=loaded.clone(); let foreign=foreign_starting.clone(); move || { let loaded=loaded.clone(); let foreign=foreign.clone(); async move {
                Json(if foreign.load(Ordering::SeqCst) {
                    serde_json::json!({"running":[{"model":"other","state":"starting"}]})
                } else if loaded.load(Ordering::SeqCst) {
                    serde_json::json!({"running":[{"model":"qwen","state":"ready","proxy":"http://127.0.0.1:9999"}]})
                } else { serde_json::json!({"running":[]}) })
            }}}))
            .route("/upstream/qwen/v1/models", get({ let loaded=loaded.clone(); let loads=loads.clone(); move || {
                let loaded=loaded.clone(); let loads=loads.clone(); async move {
                    loaded.store(true,Ordering::SeqCst); loads.fetch_add(1,Ordering::SeqCst); "ready"
                }
            }}))
            .fallback({ let forbidden=forbidden.clone(); move || { let forbidden=forbidden.clone(); async move {
                forbidden.fetch_add(1,Ordering::SeqCst); axum::http::StatusCode::NOT_FOUND
            }}});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory =
            std::env::temp_dir().join(format!("relay-auto-admission-{}", uuid::Uuid::new_v4()));
        let mut config = FleetConfig::load_or_create(&directory, "WORKSTATION").unwrap();
        config.llama_swap.listen_address = address.to_string();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let telemetry = Arc::new(TelemetryStore::new(&directory).unwrap());
        let supervisor = Arc::new(
            LlamaSwapSupervisor::new(&config.llama_swap, &directory, fleet.clone(), telemetry)
                .unwrap(),
        );
        supervisor.sync_inventory().await.unwrap();
        foreign_starting.store(true, Ordering::SeqCst);
        assert!(supervisor
            .prepare_auto("qwen", "v1/chat/completions")
            .await
            .is_err());
        assert_eq!(
            loads.load(Ordering::SeqCst),
            0,
            "Do not evict a model that is still starting"
        );
        foreign_starting.store(false, Ordering::SeqCst);
        let (endpoint, admission) = supervisor
            .prepare_inference("qwen", "v1/chat/completions", true)
            .await
            .unwrap();
        assert_eq!(endpoint, format!("http://{address}/v1/chat/completions"));
        // Switching off is immediate even while an admitted load holds its guard.
        supervisor.set_available(false).await.unwrap();
        assert!(!fleet.local_peer_status().available);
        drop(admission);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        supervisor.set_available(true).await.unwrap();
        fleet.update_local_gpu_activity(crate::gpu_activity::GpuActivity {
            busy: true,
            external_percent: Some(85.0),
            sampled_at_ms: crate::fleet::now_ms(),
        });
        assert!(supervisor
            .prepare_auto("qwen", "v1/chat/completions")
            .await
            .unwrap_err()
            .contains("GPU is busy"));
        // Pinned routes remain an explicit override; GPU load is not manual Off.
        drop(
            supervisor
                .prepare_inference("qwen", "v1/chat/completions", false)
                .await
                .unwrap(),
        );
        assert!(fleet.local_peer_status().available);
        fleet.update_local_gpu_activity(crate::gpu_activity::GpuActivity::default());
        assert!(supervisor
            .prepare_inference("other", "v1/chat/completions", true)
            .await
            .err()
            .unwrap()
            .contains("no longer idle"));
        supervisor.set_available(false).await.unwrap();
        assert!(!fleet.local_peer_status().available);
        assert!(
            loaded.load(Ordering::SeqCst),
            "Switching off must not unload existing work"
        );
        assert!(supervisor
            .prepare_inference("qwen", "v1/chat/completions", false)
            .await
            .is_err());
        assert!(
            supervisor.load_model("qwen", true).await.is_err(),
            "Force must not bypass unavailable"
        );
        let restored = FleetConfig::load_or_create(&directory, "WORKSTATION").unwrap();
        assert!(!restored.available);
        assert!(
            !FleetService::new(restored, directory.clone())
                .unwrap()
                .local_peer_status()
                .available
        );
        supervisor.set_available(true).await.unwrap();
        drop(
            supervisor
                .prepare_inference("qwen", "v1/chat/completions", true)
                .await
                .unwrap(),
        );
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "Warm re-entry must not reload"
        );
        assert_eq!(
            forbidden.load(Ordering::SeqCst),
            0,
            "No unload, cancel or inference during preparation"
        );
        server.abort();
    }

    #[tokio::test]
    async fn reclaim_drains_native_slots_preserves_embeddings_and_cancellation_releases_gate() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let loaded = Arc::new(Mutex::new("ocr".to_owned()));
        let native_busy = Arc::new(AtomicBool::new(true));
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new()
            .route("/v1/models", get({let loaded=loaded.clone(); move || {let loaded=loaded.clone(); async move {
                let current=loaded.lock().unwrap().clone();
                Json(serde_json::json!({"data":[
                    {"id":"ocr","status":{"value":if current=="ocr" {"loaded"} else {"unloaded"}},"meta":{"llamaswap":{"runtime":"llama.cpp","context_slots":6,"priority":"background"}}},
                    {"id":"chat","status":{"value":if current=="chat" {"loaded"} else {"unloaded"}},"meta":{"llamaswap":{"runtime":"llama.cpp"}}},
                    {"id":"embed","status":{"value":"loaded"},"meta":{"llamaswap":{"capabilities":["embeddings"],"resource_pool":"embeddings"}}}
                ]}))
            }}}))
            .route("/running", get({let loaded=loaded.clone(); move || {let loaded=loaded.clone(); async move {
                let current=loaded.lock().unwrap().clone();
                let mut running=vec![serde_json::json!({"model":"embed","state":"ready","proxy":format!("http://{address}")})];
                if !current.is_empty() {running.push(serde_json::json!({"model":current,"state":"ready","proxy":format!("http://{address}")}));}
                Json(serde_json::json!({"running":running}))
            }}}))
            .route("/slots", get({let busy=native_busy.clone(); move || {let busy=busy.clone(); async move {
                Json(serde_json::json!([{"id":0,"n_ctx":16384,"is_processing":busy.load(Ordering::SeqCst)}]))
            }}}))
            .route("/api/models/unload/{model}", post({let loaded=loaded.clone(); let events=events.clone(); move |axum::extract::Path(model):axum::extract::Path<String>| {
                let loaded=loaded.clone(); let events=events.clone(); async move {
                    events.lock().unwrap().push(format!("unload-{model}")); *loaded.lock().unwrap()=String::new(); "ok"
                }
            }}))
            .route("/upstream/chat/v1/models", get({let loaded=loaded.clone(); let events=events.clone(); move || {
                let loaded=loaded.clone(); let events=events.clone(); async move {
                    events.lock().unwrap().push("load-chat".into()); *loaded.lock().unwrap()="chat".into(); "ok"
                }
            }}));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory =
            std::env::temp_dir().join(format!("relay-reclaim-{}", uuid::Uuid::new_v4()));
        let mut config = FleetConfig::load_or_create(&directory, "WORKSTATION").unwrap();
        config.llama_swap.listen_address = address.to_string();
        assert!(config.routing.reclaimable_models.is_empty());
        fs::write(
            directory.join("fleet.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let supervisor = Arc::new(
            LlamaSwapSupervisor::new(
                &config.llama_swap,
                &directory,
                fleet.clone(),
                Arc::new(TelemetryStore::new(&directory).unwrap()),
            )
            .unwrap(),
        );
        supervisor.sync_inventory().await.unwrap();
        // Abort during native processing: neither unload nor cooldown is allowed.
        let task = tokio::spawn({
            let s = supervisor.clone();
            async move { s.reserve_auto("chat").await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while fleet.local_peer_status().reclaim.target.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(supervisor.track_inference("ocr").is_err());
        assert!(events.lock().unwrap().is_empty());
        task.abort();
        let _ = task.await;
        assert!(!fleet.local_peer_status().reclaim.reserved);
        // An already admitted page and native slot must both finish before replacement.
        let page = supervisor.track_inference("ocr").unwrap();
        let task = tokio::spawn({
            let s = supervisor.clone();
            async move { s.reserve_auto("chat").await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while fleet.local_peer_status().reclaim.target.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(page);
        assert!(events.lock().unwrap().is_empty());
        native_busy.store(false, Ordering::SeqCst);
        let ticket = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["unload-ocr", "load-chat"]);
        assert!(supervisor.track_inference("ocr").is_err());
        let chat = supervisor.admissions.claim(&ticket, "chat").unwrap();
        drop(chat);
        assert!(supervisor
            .load_model("ocr", false)
            .await
            .unwrap_err()
            .contains("host_occupied"));
        assert_eq!(fleet.local_peer_status().routing_version, 4);
        // Simulated native TTL unload opens background admission without any cooldown.
        *loaded.lock().unwrap() = String::new();
        supervisor.sync_inventory().await.unwrap();
        supervisor.check_background_residency("ocr").await.unwrap();
        assert!(!fleet.local_peer_status().reclaim.reserved);
        // A second run exceeds the drain deadline: only OCR is cancelled and unloaded.
        *loaded.lock().unwrap() = "ocr".into();
        native_busy.store(true, Ordering::SeqCst);
        events.lock().unwrap().clear();
        supervisor.sync_inventory().await.unwrap();
        let page = supervisor.track_inference("ocr").unwrap();
        let cancelled = page.cancellation();
        let ticket = supervisor.reserve_auto("chat").await.unwrap();
        assert!(*cancelled.borrow());
        assert_eq!(*events.lock().unwrap(), vec!["unload-ocr", "load-chat"]);
        assert!(supervisor.admissions.claim(&ticket, "chat").is_ok());
        server.abort();
    }

    #[test]
    fn background_wait_restarts_after_demand_goes_stale() {
        let start = Instant::now();
        let mut wait = BackgroundWait::default();
        assert!(!wait.refused(start));
        assert!(wait.refused(start + FOREGROUND_RELEASE_AFTER));
        let later = start + FOREGROUND_RELEASE_AFTER + BACKGROUND_DEMAND_STALE * 2;
        assert!(!wait.refused(later), "old demand must not release a model");
    }

    #[tokio::test]
    async fn waiting_background_releases_only_an_idle_foreground_model() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let loaded = Arc::new(Mutex::new("chat".to_owned()));
        let native_busy = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new()
            .route("/v1/models", get({let loaded=loaded.clone(); move || {let loaded=loaded.clone(); async move {
                let current=loaded.lock().unwrap().clone();
                Json(serde_json::json!({"data":[
                    {"id":"ocr","status":{"value":if current=="ocr" {"loaded"} else {"unloaded"}},"meta":{"llamaswap":{"runtime":"llama.cpp","context_slots":6,"priority":"background"}}},
                    {"id":"chat","status":{"value":if current=="chat" {"loaded"} else {"unloaded"}},"meta":{"llamaswap":{"runtime":"llama.cpp"}}},
                    {"id":"embed","status":{"value":"loaded"},"meta":{"llamaswap":{"capabilities":["embeddings"],"resource_pool":"embeddings"}}}
                ]}))
            }}}))
            .route("/running", get({let loaded=loaded.clone(); move || {let loaded=loaded.clone(); async move {
                let current=loaded.lock().unwrap().clone();
                let mut running=vec![serde_json::json!({"model":"embed","state":"ready","proxy":format!("http://{address}")})];
                if !current.is_empty() {running.push(serde_json::json!({"model":current,"state":"ready","proxy":format!("http://{address}")}));}
                Json(serde_json::json!({"running":running}))
            }}}))
            .route("/slots", get({let busy=native_busy.clone(); move || {let busy=busy.clone(); async move {
                Json(serde_json::json!([{"id":0,"n_ctx":16384,"is_processing":busy.load(Ordering::SeqCst)}]))
            }}}))
            .route("/api/models/unload/{model}", post({let loaded=loaded.clone(); let events=events.clone(); move |axum::extract::Path(model):axum::extract::Path<String>| {
                let loaded=loaded.clone(); let events=events.clone(); async move {
                    events.lock().unwrap().push(format!("unload-{model}")); *loaded.lock().unwrap()=String::new(); "ok"
                }
            }}));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory =
            std::env::temp_dir().join(format!("relay-release-{}", uuid::Uuid::new_v4()));
        let mut config = FleetConfig::load_or_create(&directory, "WORKSTATION").unwrap();
        config.llama_swap.listen_address = address.to_string();
        fs::write(
            directory.join("fleet.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let supervisor = Arc::new(
            LlamaSwapSupervisor::new(
                &config.llama_swap,
                &directory,
                fleet.clone(),
                Arc::new(TelemetryStore::new(&directory).unwrap()),
            )
            .unwrap(),
        );
        supervisor.sync_inventory().await.unwrap();
        let occupied = |r: Result<(), String>| r.unwrap_err().starts_with("host_occupied");
        // First refusal starts the wait; nothing is unloaded yet.
        assert!(occupied(supervisor.check_background_residency("ocr").await));
        tokio::time::sleep(FOREGROUND_RELEASE_AFTER + Duration::from_millis(50)).await;
        // An admitted interactive request keeps the model resident.
        let reply = supervisor.track_inference("chat").unwrap();
        assert!(supervisor.check_background_residency("ocr").await.is_err());
        drop(reply);
        // Work that bypassed Relay but occupies a native slot also keeps it resident.
        native_busy.store(true, Ordering::SeqCst);
        assert!(occupied(supervisor.check_background_residency("ocr").await));
        assert!(events.lock().unwrap().is_empty());
        // Idle after the wait: only the foreground model is unloaded, never embeddings.
        native_busy.store(false, Ordering::SeqCst);
        assert!(occupied(supervisor.check_background_residency("ocr").await));
        assert_eq!(*events.lock().unwrap(), vec!["unload-chat"]);
        // The release is recorded so an unexpected unload can be explained later.
        let lifecycle = supervisor.telemetry.summary(1).unwrap().recent_lifecycle;
        assert!(lifecycle
            .iter()
            .any(|e| e.action == "release_for_background"
                && e.model_id.as_deref() == Some("chat")
                && e.outcome == "applied"));
        // The background retry is admitted with no cooldown.
        supervisor.check_background_residency("ocr").await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn capacity_inspection_does_not_load_idle_models_or_use_dispatch() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{routing::get, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let flag = ready.clone();
        let unexpected = Arc::new(AtomicU64::new(0));
        let calls = unexpected.clone();
        let app = Router::new()
            .route("/v1/models", get(|| async { Json(serde_json::json!({"data":[
                {"id":"vision","status":{"value":"unloaded"},"meta":{"llamaswap":{"runtime":"llama.cpp"}}}
            ]})) }))
            .route("/running", get(move || { let flag = flag.clone(); async move {
                Json(if flag.load(Ordering::SeqCst) {
                    serde_json::json!({"running":[{"model":"vision","state":"ready","proxy":format!("http://{address}")}]})
                } else { serde_json::json!({"running":[]}) })
            }}))
            .route("/slots", get(|| async { Json(serde_json::json!([
                {"id":0,"n_ctx":16384,"is_processing":true,"speculative":true}
            ])) }))
            .fallback(move || { let calls = calls.clone(); async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::NOT_FOUND
            }});
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory =
            std::env::temp_dir().join(format!("relay-capacity-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let mut config = FleetConfig::defaults("Test");
        config.llama_swap.listen_address = address.to_string();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let telemetry = Arc::new(TelemetryStore::new(&directory).unwrap());
        let supervisor =
            LlamaSwapSupervisor::new(&config.llama_swap, &directory, fleet, telemetry).unwrap();
        supervisor.sync_inventory().await.unwrap();
        let idle = supervisor.model_capacity("vision").await.unwrap();
        assert_eq!(idle["loaded"], false);
        assert!(idle["total_slots"].is_null());
        ready.store(true, Ordering::SeqCst);
        let active = supervisor.model_capacity("vision").await.unwrap();
        assert_eq!(active["busy_slots"], 1);
        assert_eq!(active["slots"][0]["context_window"], 16384);
        assert!(supervisor.model_capacity("unknown").await.is_err());
        assert_eq!(unexpected.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[test]
    fn multi_slot_context_preserves_per_request_metadata() {
        let directory =
            std::env::temp_dir().join(format!("relay-slot-context-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("llama-swap.yaml");
        let fixture = "models:\n  vision:\n    cmd: llama-server --ctx-size 65536 --parallel 4\n    metadata:\n      context_slots: 4\n      context_length: 16384\n";
        fs::write(&path, fixture).unwrap();
        let plan = prepare_model_context(&path, "vision", 32768).unwrap();
        assert!(plan.updated.contains("--ctx-size 131072 --parallel 4"));
        assert!(plan.updated.contains("context_length: 32768"));
        fs::write(
            &path,
            fixture.replace("context_slots: 4", "context_slots: 0"),
        )
        .unwrap();
        assert!(prepare_model_context(&path, "vision", 32768).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn rewrites_only_the_selected_model_context() {
        let directory =
            std::env::temp_dir().join(format!("agentrelay-context-rewrite-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create context rewrite directory");
        let path = directory.join("llama-swap.yaml");
        fs::write(
            &path,
            concat!(
                "# keep this comment\n",
                "models:\n",
                "  qwen:\n",
                "    cmd: llama-server --ctx-size 65536 --parallel 1\n",
                "    metadata:\n",
                "      context_length: 65536\n",
                "  ornith:\n",
                "    cmd: llama-server --ctx-size=32768 --parallel 1\n",
                "    metadata:\n",
                "      runtime: llama.cpp\n",
            ),
        )
        .expect("write context fixture");

        assert!(rewrite_model_context(&path, "ornith", 262_144).expect("rewrite selected context"));
        let updated = fs::read_to_string(&path).expect("read rewritten context");
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("qwen:\n    cmd: llama-server --ctx-size 65536"));
        assert!(updated.contains("ornith:\n    cmd: llama-server --ctx-size=262144"));
        assert!(
            updated.contains("metadata:\n      context_length: 262144\n      runtime: llama.cpp")
        );
        fs::remove_file(&path).expect("remove context fixture");
        fs::remove_dir(&directory).expect("remove context rewrite directory");
    }

    #[test]
    fn rewrites_short_and_environment_context_arguments() {
        assert_eq!(
            replace_cli_number(
                "    cmd: llama-server -c 65536 --parallel 1",
                "-c",
                "131072"
            ),
            Some("    cmd: llama-server -c 131072 --parallel 1".into())
        );
        assert_eq!(
            replace_cli_number("    cmd: llama-server -c=65536", "-c", "131072"),
            Some("    cmd: llama-server -c=131072".into())
        );
        assert_eq!(
            replace_cli_number(
                "    cmd: LLAMA_ARG_CTX_SIZE=65536 llama-server",
                "LLAMA_ARG_CTX_SIZE",
                "131072"
            ),
            Some("    cmd: LLAMA_ARG_CTX_SIZE=131072 llama-server".into())
        );
        assert_eq!(
            replace_cli_number("    cmd: llama-server --cache-type-k q8_0", "-c", "131072"),
            None
        );
    }

    #[test]
    fn strata_context_uses_an_isolated_copy_and_keeps_source_updates() {
        let directory = std::env::temp_dir().join(format!("relay-strata-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("original vision.json");
        let original = serde_json::json!({"exe":"strata", "args":["--max-context","131072","--kv-resident","32768"], "vision":{"max_tokens":1024}});
        fs::write(&source, serde_json::to_string_pretty(&original).unwrap()).unwrap();
        let path = directory.join("llama-swap.yaml");
        let source_arg = source.to_string_lossy().replace('\\', "/");
        let yaml = format!("# preserve comments\r\nmodels:\r\n  vision:\r\n    cmd: >-\r\n      python server.py --engine strata --config \"{source_arg}\"\r\n      --port ${{PORT}}\r\n    metadata:\r\n      runtime: strata\r\n      context_length: 131072\r\n  other:\r\n    cmd: python server.py --config \"{source_arg}\"\r\n    metadata:\r\n      runtime: strata\r\n      context_length: 131072\r\n");
        fs::write(&path, &yaml).unwrap();
        let plan = prepare_model_context(&path, "vision", 262144).unwrap();
        assert!(
            !directory.join("runtime-context").exists(),
            "preflight must not write files"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), yaml);
        assert!(plan.apply(&path).unwrap());
        let managed = directory.join("runtime-context/vision.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(&managed).unwrap()).unwrap();
        assert_eq!(value["args"][1], "262144");
        assert_eq!(value["args"][3], "32768");
        assert_eq!(value["vision"], original["vision"]);
        let unchanged: Value = serde_json::from_str(&fs::read_to_string(&source).unwrap()).unwrap();
        assert_eq!(unchanged, original);
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with("# preserve comments\r\n"));
        assert_eq!(
            updated.split("  other:").nth(1),
            yaml.split("  other:").nth(1)
        );
        assert!(!rewrite_model_context(&path, "vision", 262144).unwrap());
        let mut new_source = original;
        new_source["vision"]["max_tokens"] = serde_json::json!(2048);
        fs::write(&source, serde_json::to_string_pretty(&new_source).unwrap()).unwrap();
        assert!(rewrite_model_context(&path, "vision", 65536).unwrap());
        let value: Value = serde_json::from_str(&fs::read_to_string(&managed).unwrap()).unwrap();
        assert_eq!(value["args"][1], "65536");
        assert_eq!(value["vision"]["max_tokens"], 2048);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn invalid_context_does_not_unload_or_save_an_override() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let unloaded = Arc::new(AtomicBool::new(false));
        let flag = unloaded.clone();
        let app = Router::new()
            .route("/v1/models", get(|| async { Json(serde_json::json!({"data":[
                {"id":"working","status":{"value":"loaded"}},
                {"id":"broken","status":{"value":"unloaded"},"meta":{"llamaswap":{"runtime":"strata","context_length":131072}}}
            ]})) }))
            .route("/running", get(|| async { Json(serde_json::json!({"running":[{"model":"working","state":"ready","proxy":"http://127.0.0.1:9999"}]})) }))
            .route("/api/models/unload", post(move || { let flag=flag.clone(); async move { flag.store(true,Ordering::SeqCst); "ok" } }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory =
            std::env::temp_dir().join(format!("relay-context-preflight-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let config: FleetConfig = serde_json::from_value(serde_json::json!({
            "schema_version":1,"local_host_id":"test","poll_interval_seconds":5,"request_timeout_ms":1000,"peer_api_port":38473,
            "hosts":[{"id":"test","display_name":"Test","address":"127.0.0.1","hardware":"test"}],
            "llama_swap":{"listen_address":address.to_string(),"config_path":"llama-swap.yaml"}
        })).unwrap();
        fs::write(
            directory.join("fleet.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let yaml="models:\n  broken:\n    cmd: python server.py --config relative.json\n    metadata:\n      runtime: strata\n      context_length: 131072\n";
        fs::write(directory.join("llama-swap.yaml"), yaml).unwrap();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let telemetry = Arc::new(TelemetryStore::new(&directory).unwrap());
        let supervisor = Arc::new(
            LlamaSwapSupervisor::new(&config.llama_swap, &directory, fleet.clone(), telemetry)
                .unwrap(),
        );
        supervisor.sync_inventory().await.unwrap();
        for force in [false, true] {
            let error = supervisor
                .load_model_with_context("broken", force, Some(65536))
                .await
                .unwrap_err();
            assert!(error.contains("absolute --config"), "{error}");
            assert!(!unloaded.load(Ordering::SeqCst));
            assert!(config::get_model_context_overrides(&directory)
                .unwrap()
                .is_empty());
            assert_eq!(
                fs::read_to_string(directory.join("llama-swap.yaml")).unwrap(),
                yaml
            );
        }
        server.abort();
        drop(supervisor);
        drop(fleet);
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn cpu_embeddings_coexist_with_chat_and_reload_independently() {
        use crate::{config::FleetConfig, fleet::FleetService, telemetry::TelemetryStore};
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let loaded = serde_json::json!({"data":[
            {"id":"embed","status":{"value":"loaded"},"meta":{"llamaswap":{"kind":"text","capabilities":["embeddings"],"resource_pool":"cpu"}}},
            {"id":"chat","status":{"value":"loaded"}},
            {"id":"next-chat","status":{"value":"unloaded"}}
        ]});
        let c = calls.clone();
        let d = calls.clone();
        let e = calls.clone();
        let next_calls = calls.clone();
        let app = Router::new()
            .route(
                "/upstream/next-chat/v1/models",
                get(move || {
                    let c = next_calls.clone();
                    async move {
                        c.lock().unwrap().push("load-next-chat".into());
                        "ok"
                    }
                }),
            )
            .route(
                "/v1/models",
                get(move || {
                    let v = loaded.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/running",
                get(|| async {
                    Json(serde_json::json!({"running":[{"model":"embed"},{"model":"chat"}]}))
                }),
            )
            .route(
                "/upstream/embed/v1/models",
                get(move || {
                    let c = c.clone();
                    async move {
                        c.lock().unwrap().push("load-embed".into());
                        "ok"
                    }
                }),
            )
            .route(
                "/api/models/unload/{model}",
                post(
                    move |axum::extract::Path(model): axum::extract::Path<String>| {
                        let d = d.clone();
                        async move {
                            d.lock().unwrap().push(format!("unload-{model}"));
                            "ok"
                        }
                    },
                ),
            )
            .route(
                "/api/models/unload",
                post(move || {
                    let e = e.clone();
                    async move {
                        e.lock().unwrap().push("unload-all".into());
                        "ok"
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory =
            std::env::temp_dir().join(format!("relay-cpu-embedding-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let config: FleetConfig=serde_json::from_value(serde_json::json!({
            "schema_version":1,"local_host_id":"test","poll_interval_seconds":5,"request_timeout_ms":1000,"peer_api_port":38473,
            "hosts":[{"id":"test","display_name":"Test","address":"127.0.0.1","hardware":"test"}],
            "llama_swap":{"listen_address":address.to_string(),"config_path":"llama-swap.yaml"}
        })).unwrap();
        fs::write(
            directory.join("fleet.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let fleet = Arc::new(FleetService::new(config.clone(), directory.clone()).unwrap());
        let telemetry = Arc::new(TelemetryStore::new(&directory).unwrap());
        let supervisor = Arc::new(
            LlamaSwapSupervisor::new(&config.llama_swap, &directory, fleet.clone(), telemetry)
                .unwrap(),
        );
        supervisor.sync_inventory().await.unwrap();
        assert_eq!(
            supervisor
                .local_host_status()
                .unwrap()
                .loaded_model_id
                .as_deref(),
            Some("chat")
        );
        supervisor.inflight.write().unwrap().push(InflightRequest {
            id: "active-chat".into(),
            model: "chat".into(),
        });
        assert_eq!(
            supervisor.load_model("embed", false).await.unwrap().state,
            ControlState::Applied
        );
        assert_eq!(*calls.lock().unwrap(), vec!["load-embed"]);
        calls.lock().unwrap().clear();
        supervisor.reload_model("embed", true, None).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["unload-embed", "load-embed"]);
        assert_eq!(supervisor.inflight.read().unwrap().len(), 1);
        calls.lock().unwrap().clear();
        let profiles = supervisor.local_host_status().unwrap().models;
        let embedding = profiles.iter().find(|p| p.id == "embed").unwrap();
        assert!(embedding.is_independent_embedding_service());
        assert!(!embedding.supports_openai_generation());
        let mut gpu = embedding.clone();
        gpu.resource_pool = "gpu0".into();
        assert!(!gpu.is_independent_embedding_service());
        gpu.resource_pool = "embeddings".into();
        assert!(gpu.is_independent_embedding_service());
        let mut mixed = embedding.clone();
        mixed.capabilities.push(ProfileCapability::Chat);
        assert!(!mixed.is_independent_embedding_service());
        supervisor
            .unload_foreground_models(&profiles)
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["unload-chat"]);
        calls.lock().unwrap().clear();
        *supervisor.inflight.write().unwrap() = vec![InflightRequest {
            id: "active-embedding".into(),
            model: "embed".into(),
        }];
        assert_eq!(
            supervisor
                .load_model("next-chat", false)
                .await
                .unwrap()
                .state,
            ControlState::Applied
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["unload-chat", "load-next-chat"]
        );
        server.abort();
        drop(supervisor);
        drop(fleet);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn reapplies_persisted_context_after_generated_config_changes() {
        let directory = std::env::temp_dir().join(format!(
            "agent-relay-generated-context-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create generated context directory");
        let path = directory.join("llama-swap.yaml");
        let generated = concat!(
            "models:\n",
            "  qwen:\n",
            "    cmd: llama-server -c 65536\n",
            "    metadata:\n",
            "      context_length: 65536\n"
        );
        fs::write(&path, generated).expect("write generated config");
        let overrides = BTreeMap::from([
            ("m1-pro/qwen".to_owned(), 131_072),
            ("workstation/ornith".to_owned(), 262_144),
        ]);

        assert!(reapply_model_context_overrides(&path, "m1-pro", &overrides)
            .expect("reapply generated context"));
        let updated = fs::read_to_string(&path).expect("read updated generated config");
        assert!(updated.contains("llama-server -c 131072"));
        assert!(updated.contains("context_length: 131072"));
        assert!(
            !reapply_model_context_overrides(&path, "m1-pro", &overrides)
                .expect("idempotent context reapply")
        );

        fs::remove_file(path).expect("remove generated config");
        fs::remove_dir(directory).expect("remove generated context directory");
    }

    #[test]
    fn reads_context_length_from_llama_swap_metadata() {
        let meta = serde_json::json!({"llamaswap": {"context_length": 262144}});
        assert_eq!(model_context_length(&Some(meta)), Some(262_144));
        assert_eq!(model_context_length(&None), None);
    }

    #[test]
    fn manages_only_profiles_with_a_fixed_advertised_context() {
        assert_eq!(
            managed_context_request(Some(65_536), Some(262_144)),
            Some(262_144)
        );
        assert_eq!(managed_context_request(None, Some(262_144)), None);
        assert_eq!(managed_context_request(Some(65_536), None), None);
    }

    #[test]
    fn resolves_ready_models_through_llama_swap_process_tracking() {
        let running = vec![serde_json::json!({
            "model": "qwen",
            "state": "ready",
            "proxy": "http://127.0.0.1:5806/"
        })];
        assert_eq!(
            LlamaSwapSupervisor::ready_tracked_endpoint(
                "http://127.0.0.1:38474",
                &running,
                "qwen",
                "v1/chat/completions?trace=1"
            )
            .unwrap()
            .as_deref(),
            Some("http://127.0.0.1:38474/v1/chat/completions?trace=1")
        );
        assert!(LlamaSwapSupervisor::ready_tracked_endpoint(
            "http://127.0.0.1:38474",
            &running,
            "another-model",
            "v1/models"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn tracked_route_does_not_expose_the_child_proxy() {
        let running = vec![serde_json::json!({
            "model": "qwen 3.8/iq4xs",
            "state": "ready",
            "proxy": "http://127.0.0.1:5806"
        })];
        assert_eq!(
            LlamaSwapSupervisor::ready_tracked_endpoint(
                "http://127.0.0.1:38474/",
                &running,
                "qwen 3.8/iq4xs",
                "/v1/models"
            )
            .unwrap()
            .as_deref(),
            Some("http://127.0.0.1:38474/v1/models")
        );
    }

    #[test]
    fn expired_profile_clears_the_loaded_model() {
        let loaded: ModelsResponse = serde_json::from_value(serde_json::json!({
            "data": [{
                "id": "ornith",
                "name": "Ornith",
                "meta": null,
                "status": { "value": "loaded" }
            }]
        }))
        .unwrap();
        assert_eq!(loaded_model_id(&loaded.data).as_deref(), Some("ornith"));

        let expired: ModelsResponse = serde_json::from_value(serde_json::json!({
            "data": [{
                "id": "ornith",
                "name": "Ornith",
                "meta": null,
                "status": { "value": "unloaded" }
            }]
        }))
        .unwrap();
        assert_eq!(loaded_model_id(&expired.data), None);
    }

    #[test]
    fn reads_runtime_from_llama_swap_metadata() {
        let meta = serde_json::json!({"llamaswap": {"runtime": "mlx"}});
        assert_eq!(model_runtime(&Some(meta)), "mlx");
        assert_eq!(model_runtime(&None), "unspecified");
    }

    #[test]
    fn voice_metadata_does_not_advertise_chat_capabilities() {
        let meta =
            Some(serde_json::json!({"llamaswap": {"kind":"voice", "runtime":"mlx_voicechat"}}));
        assert_eq!(model_kind(&meta), WorkloadKind::Voice);
        assert_eq!(
            model_capabilities(&meta),
            vec![ProfileCapability::RealtimeAudio]
        );
    }

    #[test]
    fn reads_worker_kind_and_http_service_capability_from_metadata() {
        let meta = serde_json::json!({
            "llamaswap": {
                "runtime": "python",
                "kind": "worker",
                "capabilities": ["http_service"],
                "resource_pool": "gpu0"
            }
        });
        assert_eq!(model_kind(&Some(meta.clone())), WorkloadKind::Worker);
        assert_eq!(
            model_capabilities(&Some(meta.clone())),
            vec![ProfileCapability::HttpService]
        );
        let bare = serde_json::json!({ "llamaswap": { "kind": "worker" } });
        assert_eq!(
            model_capabilities(&Some(bare)),
            vec![ProfileCapability::HttpService]
        );
    }

    #[test]
    fn reads_capabilities_and_pool_from_llama_swap_metadata() {
        let meta = serde_json::json!({"llamaswap": {
            "kind": "image",
            "capabilities": ["image_generation", "workflow_queue"],
            "resource_pool": "gpu0"
        }});

        assert_eq!(model_kind(&Some(meta.clone())), WorkloadKind::Image);
        assert_eq!(
            model_capabilities(&Some(meta.clone())),
            vec![
                ProfileCapability::ImageGeneration,
                ProfileCapability::WorkflowQueue
            ]
        );
        assert_eq!(model_resource_pool(&Some(meta)), "gpu0");
    }

    #[test]
    fn explicit_vision_flag_enables_auto_for_legacy_strata_metadata() {
        let meta = Some(serde_json::json!({"llamaswap":{"runtime":"strata","vision":true}}));
        assert!(model_capabilities(&meta).contains(&ProfileCapability::VisionInput));
        let text = Some(serde_json::json!({"llamaswap":{"runtime":"strata","vision":false}}));
        assert!(!model_capabilities(&text).contains(&ProfileCapability::VisionInput));
    }

    #[test]
    fn reads_inference_controls_from_llama_swap_metadata() {
        let meta = serde_json::json!({"llamaswap": {
            "inference_controls": {
                "thinking": {
                    "adapter": "llama_cpp",
                    "efforts": ["off", "low", "xhigh"],
                    "default_effort": "low",
                    "budget_min": -1,
                    "budget_max": 16384,
                    "budget_step": 256,
                    "default_budget": -1
                },
                "temperature": {"min": 0.0, "max": 2.0, "step": 0.05, "default": 0.3}
            }
        }});
        let controls = model_inference_controls(&Some(meta));
        let thinking = controls.thinking.expect("thinking controls");
        assert_eq!(
            thinking.default_effort,
            Some(crate::domain::ReasoningEffort::Low)
        );
        assert_eq!(thinking.default_budget, Some(-1));
        assert_eq!(
            controls.temperature.expect("temperature").default,
            Some(0.3)
        );
    }

    #[test]
    fn reads_on_as_a_distinct_template_toggle() {
        let meta = serde_json::json!({"llamaswap": {
            "inference_controls": {
                "thinking": {
                    "adapter": "mlx_toggle",
                    "efforts": ["off", "on"],
                    "default_effort": "off",
                    "budget_min": -1,
                    "budget_max": 16384,
                    "budget_step": 256,
                    "default_budget": 1024
                }
            }
        }});
        let thinking = model_inference_controls(&Some(meta))
            .thinking
            .expect("thinking controls");
        assert_eq!(
            thinking.efforts,
            vec![
                crate::domain::ReasoningEffort::Off,
                crate::domain::ReasoningEffort::On
            ]
        );
        assert_eq!(thinking.default_budget, Some(1024));
    }

    #[test]
    fn probes_a_real_openai_endpoint_when_loading_a_model() {
        assert_eq!(
            model_probe_url("http://127.0.0.1:38474", "bonsai 27b/q4"),
            "http://127.0.0.1:38474/upstream/bonsai%2027b%2Fq4/v1/models"
        );
    }

    #[test]
    fn applies_inflight_snapshots_updates_and_removals() {
        let mut requests = Vec::new();
        apply_inflight_update(
            &mut requests,
            InflightUpdate {
                operation: "snapshot".into(),
                requests: vec![InflightRequest {
                    id: "one".into(),
                    model: "qwen".into(),
                }],
                request: None,
                id: None,
            },
        );
        assert_eq!(requests.len(), 1);

        apply_inflight_update(
            &mut requests,
            InflightUpdate {
                operation: "remove".into(),
                requests: Vec::new(),
                request: None,
                id: Some("one".into()),
            },
        );
        assert!(requests.is_empty());
    }

    #[test]
    fn inflight_cancellation_is_bounded_when_the_server_stalls() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind stalled server");
        let endpoint = format!("http://{}", listener.local_addr().expect("server address"));
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept cancellation");
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n")
                .expect("write stalled response headers");
            connection.flush().expect("flush response headers");
            thread::sleep(Duration::from_millis(250));
        });
        let client = reqwest::Client::builder()
            .build()
            .expect("build control client");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let started = Instant::now();
        runtime.block_on(cancel_inflight_requests(
            &client,
            &endpoint,
            vec![InflightRequest {
                id: "request-1".into(),
                model: "qwen".into(),
            }],
            Duration::from_millis(20),
            Duration::from_millis(50),
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn stale_child_events_do_not_match_a_restarted_process() {
        assert!(child_event_matches(7, Some(200), 7, 200));
        assert!(!child_event_matches(8, Some(300), 7, 200));
        assert!(!child_event_matches(7, Some(300), 7, 200));
        assert!(!child_event_matches(7, None, 7, 200));
    }

    #[test]
    fn resolves_the_packaged_sidecar_beside_the_app_executable() {
        let executable = Path::new("/Applications/Agent Relay.app/Contents/MacOS/agent-relay");
        let resolved = sidecar_path_from_executable(executable).expect("sidecar path");
        assert_eq!(resolved.parent(), executable.parent());
        assert_eq!(
            resolved.file_name().and_then(|name| name.to_str()),
            Some(if cfg!(windows) {
                "llama-swap.exe"
            } else {
                "llama-swap"
            })
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn independent_process_survives_dropping_the_parent_handle() {
        let temporary = std::env::temp_dir().join(format!(
            "agentrelay-independent-process-{}",
            std::process::id()
        ));
        fs::create_dir_all(&temporary).expect("create process test directory");
        let started = temporary.join("started");
        let finished = temporary.join("finished");
        let _ = fs::remove_file(&started);
        let _ = fs::remove_file(&finished);

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            let script = format!(
                "Set-Content -LiteralPath '{}' -Value started; Start-Sleep -Milliseconds 600; Set-Content -LiteralPath '{}' -Value finished",
                started.display(),
                finished.display()
            );
            command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            command
        };
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            let script = format!(
                "touch '{}'; sleep 0.6; touch '{}'",
                started.display(),
                finished.display()
            );
            command.args(["-c", &script]);
            command
        };

        configure_independent_process(&mut command);
        let child = command.spawn().expect("start independent process");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !started.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(started.exists());
        drop(child);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !finished.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(finished.exists());
        let _ = fs::remove_dir_all(temporary);
    }

    #[test]
    fn recognizes_fixed_and_chunked_llama_swap_running_responses() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"running\":[]}";
        assert!(parse_json_http_response(fixed)
            .and_then(|value| value.get("running").cloned())
            .is_some_and(|running| running.is_array()));

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\ne\r\n{\"running\":[]}\r\n0\r\n\r\n";
        assert!(parse_json_http_response(chunked)
            .and_then(|value| value.get("running").cloned())
            .is_some_and(|running| running.is_array()));
        assert!(
            parse_json_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").is_some()
        );
        assert!(
            parse_json_http_response(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}")
                .is_none()
        );
    }

    #[test]
    fn accepts_only_llama_swap_process_names() {
        assert!(looks_like_llama_swap_process("llama-swap.exe"));
        assert!(looks_like_llama_swap_process(
            "/Applications/Agent Relay.app/Contents/MacOS/llama-swap"
        ));
        assert!(looks_like_llama_swap_process(
            "llama-swap-x86_64-pc-windows-msvc.exe"
        ));
        assert!(!looks_like_llama_swap_process("llama-server.exe"));
        assert!(!looks_like_llama_swap_process("not-llama-swap.exe"));
    }

    #[test]
    fn finds_an_adopted_llama_swap_by_its_listen_argument() {
        let args = |listen: &str| {
            vec![
                "llama-swap.exe".to_owned(),
                "--config".into(),
                "C:\\relay\\llama-swap.yaml".into(),
                "--listen".into(),
                listen.into(),
                "--watch-config".into(),
            ]
        };
        let processes = vec![
            (6796, "llama-swap.exe".to_owned(), args("127.0.0.1:38474")),
            (7000, "llama-swap.exe".to_owned(), args("127.0.0.1:38499")),
            // Another program passing the same address is never mistaken for llama-swap.
            (7100, "llama-server.exe".to_owned(), args("127.0.0.1:38474")),
        ];
        assert_eq!(
            pid_from_listen_argument(&processes, "127.0.0.1:38474"),
            Some(6796)
        );
        assert_eq!(pid_from_listen_argument(&processes, "127.0.0.1:1"), None);
        // Two llama-swap processes claiming one endpoint are ambiguous: refuse.
        let mut twice = processes.clone();
        twice.push((6800, "llama-swap.exe".to_owned(), args("127.0.0.1:38474")));
        assert_eq!(pid_from_listen_argument(&twice, "127.0.0.1:38474"), None);
    }

    #[test]
    fn requires_a_unique_listener_pid() {
        assert_eq!(unique_pid(vec![42, 42]), Some(42));
        assert_eq!(unique_pid(vec![42, 43]), None);
        assert_eq!(unique_pid(Vec::new()), None);
    }

    #[cfg(windows)]
    #[test]
    fn parses_only_the_requested_windows_listener_port() {
        let output = concat!(
            "  TCP    127.0.0.1:38474      0.0.0.0:0      LISTENING       1234\n",
            "  TCP    127.0.0.1:38475      0.0.0.0:0      LISTENING       5678\n",
            "  TCP    127.0.0.1:38474      127.0.0.1:50000 ESTABLISHED     1234\n",
        );
        assert_eq!(parse_netstat_listener_pids(output, 38_474), vec![1234]);
        assert_eq!(socket_field_port("[::1]:38474"), Some(38_474));
    }
}
