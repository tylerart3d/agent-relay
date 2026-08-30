use std::{
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

pub struct LlamaSwapSupervisor {
    endpoint: String,
    config_path: PathBuf,
    client: reqwest::Client,
    control_client: reqwest::Client,
    child: Mutex<Option<Child>>,
    status: RwLock<LlamaSwapStatus>,
    fleet: SharedFleetService,
    inflight: RwLock<Vec<InflightRequest>>,
    generation: AtomicU64,
    adopted: AtomicBool,
    lifecycle: tokio::sync::Mutex<()>,
    telemetry: SharedTelemetry,
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
            fleet,
            inflight: RwLock::new(Vec::new()),
            generation: AtomicU64::new(0),
            adopted: AtomicBool::new(false),
            lifecycle: tokio::sync::Mutex::new(()),
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

    pub async fn load_model_with_context(
        self: &Arc<Self>,
        model_id: &str,
        force: bool,
        context_window: Option<u32>,
    ) -> Result<ControlOutcome, String> {
        let started = Instant::now();
        let _lifecycle_guard = self.lifecycle.lock().await;
        let result = self.load_model_inner(model_id, force, context_window).await;
        self.record_lifecycle("load", Some(model_id), force, started, &result);
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

        Self::ready_proxy_endpoint(&response.running, model_id, path_and_query)
    }

    fn ready_proxy_endpoint(
        running_models: &[Value],
        model_id: &str,
        path_and_query: &str,
    ) -> Result<Option<String>, String> {
        let proxy = running_models.iter().find_map(|running| {
            let is_ready = running.get("model").and_then(Value::as_str) == Some(model_id)
                && running.get("state").and_then(Value::as_str) == Some("ready");
            is_ready
                .then(|| running.get("proxy").and_then(Value::as_str))
                .flatten()
        });
        let Some(proxy) = proxy else {
            return Ok(None);
        };
        if !(proxy.starts_with("http://127.0.0.1:") || proxy.starts_with("http://localhost:")) {
            return Err("llama-swap reported a non-loopback upstream endpoint".to_owned());
        }
        Ok(Some(format!(
            "{}/{}",
            proxy.trim_end_matches('/'),
            path_and_query.trim_start_matches('/')
        )))
    }

    async fn load_model_inner(
        self: &Arc<Self>,
        model_id: &str,
        force: bool,
        context_window: Option<u32>,
    ) -> Result<ControlOutcome, String> {
        let local = self.local_host_status()?;
        let profile = local.models.iter().find(|model| model.id == model_id);
        let Some(profile) = profile else {
            return Err(format!("unknown local model profile: {model_id}"));
        };
        if let Some(context_window) = context_window {
            config::validate_client_context_window(context_window)?;
        }
        // Profiles that advertise a context length have a fixed launch-time
        // context that Agent Relay can manage. Runtimes such as MLX and MTPLX
        // omit it because they negotiate or bound context themselves.
        let managed_context = managed_context_request(profile.context_length, context_window);
        let context_matches =
            managed_context.is_none_or(|requested| profile.context_length == Some(requested));
        if local.loaded_model_id.as_deref() == Some(model_id) && context_matches {
            return Ok(self.outcome(ControlState::Noop, format!("{model_id} is already loaded")));
        }

        let active = self.active_count();
        if active > 0 && !force {
            return Ok(self.outcome(
                ControlState::Conflict,
                format!("{active} request(s) are currently using this host"),
            ));
        }
        if force {
            self.cancel_all_inflight().await;
            self.unload_upstream().await?;
        } else if local.loaded_model_id.is_some() {
            self.unload_upstream().await?;
        }

        if let Some(context_window) = managed_context.filter(|_| !context_matches) {
            rewrite_model_context(&self.config_path, model_id, context_window)?;
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
            .get(model_probe_url(&self.endpoint, model_id))
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

    async fn sync_inventory(&self) -> Result<(), String> {
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
        self.fleet.update_active_requests(0);
    }

    fn active_count(&self) -> u32 {
        self.inflight
            .read()
            .expect("inflight requests poisoned")
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
        let active = inflight.len() as u32;
        drop(inflight);
        self.fleet.update_active_requests(active);
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
    platform_listener_pid(port)
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

fn rewrite_model_context(path: &Path, model_id: &str, context_window: u32) -> Result<(), String> {
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

    let replacement = context_window.to_string();
    let mut command_updated = false;
    let mut metadata_updated = false;
    for line in &mut lines[start + 1..end] {
        if let Some(updated) = replace_cli_number(line, "--ctx-size", &replacement) {
            *line = updated;
            command_updated = true;
        }
        if line.trim_start().starts_with("context_length:") {
            let indent = &line[..line.len() - line.trim_start().len()];
            *line = format!("{indent}context_length: {context_window}");
            metadata_updated = true;
        }
    }
    if !command_updated {
        return Err(format!(
            "model profile {model_id} has no --ctx-size launch argument"
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
    config::atomic_write_text(path, &updated)
        .map_err(|error| format!("failed to update {}: {error}", path.display()))?;
    crate::config_watch::record_internal_change(path)
        .map_err(|error| format!("failed to track update to {}: {error}", path.display()))
}

fn yaml_model_key(line: &str) -> Option<&str> {
    if !line.starts_with("  ") || line.starts_with("    ") {
        return None;
    }
    let key = line.trim().strip_suffix(':')?.trim();
    Some(key.trim_matches(|character| matches!(character, '\'' | '"')))
}

fn replace_cli_number(line: &str, option: &str, replacement: &str) -> Option<String> {
    let start = line.find(option)? + option.len();
    let bytes = line.as_bytes();
    let mut number_start = start;
    while number_start < bytes.len()
        && (bytes[number_start].is_ascii_whitespace() || bytes[number_start] == b'=')
    {
        number_start += 1;
    }
    let mut number_end = number_start;
    while number_end < bytes.len() && bytes[number_end].is_ascii_digit() {
        number_end += 1;
    }
    (number_end > number_start).then(|| {
        format!(
            "{}{}{}",
            &line[..number_start],
            replacement,
            &line[number_end..]
        )
    })
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
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !configured.is_empty() {
        return configured;
    }

    match model_kind(meta) {
        WorkloadKind::Image => vec![
            ProfileCapability::ImageGeneration,
            ProfileCapability::WorkflowQueue,
        ],
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
    }
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
        .find(|model| model.status.value == "loaded")
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

        rewrite_model_context(&path, "ornith", 262_144).expect("rewrite selected context");
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
    fn resolves_ready_models_to_their_direct_loopback_endpoint() {
        let running = vec![serde_json::json!({
            "model": "qwen",
            "state": "ready",
            "proxy": "http://127.0.0.1:5806/"
        })];
        assert_eq!(
            LlamaSwapSupervisor::ready_proxy_endpoint(
                &running,
                "qwen",
                "v1/chat/completions?trace=1"
            )
            .unwrap()
            .as_deref(),
            Some("http://127.0.0.1:5806/v1/chat/completions?trace=1")
        );
        assert!(
            LlamaSwapSupervisor::ready_proxy_endpoint(&running, "another-model", "v1/models")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_non_loopback_model_endpoints() {
        let running = vec![serde_json::json!({
            "model": "qwen",
            "state": "ready",
            "proxy": "http://example.com:5806"
        })];
        assert!(
            LlamaSwapSupervisor::ready_proxy_endpoint(&running, "qwen", "v1/models")
                .unwrap_err()
                .contains("non-loopback")
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
