use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    channels::{HarnessDeliveryRequest, HarnessDeliveryResponse, HarnessSessionArchiveRequest},
    config::{FleetConfig, HostConfig, CONFIG_FILE_NAME},
    discovery,
    domain::{
        ConnectionState, ControlOutcome, FleetSnapshot, HarnessStatus, HermesStatus,
        HermesSyncState, HostStatus, LlamaSwapStatus, LoadModelRequest, ModelProfile,
        OpenCodeStatus, OpenCodeSyncState, PeerApiStatus, PeerStatusResponse, UnloadModelsRequest,
    },
    harness_setup::{HarnessSetupRequest, HarnessSetupStatus},
};

pub const PEER_LOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const PEER_UNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
pub const PEER_HARNESS_TIMEOUT: Duration = Duration::from_secs(26 * 60);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CONTROL_RESPONSE_BYTES: usize = 1024 * 1024;

pub struct FleetService {
    config: FleetConfig,
    client: reqwest::Client,
    control_client: reqwest::Client,
    snapshot: RwLock<FleetSnapshot>,
    refresh_lock: tokio::sync::Mutex<()>,
    discovered_hosts: RwLock<Vec<HostConfig>>,
    discovered_hosts_path: PathBuf,
    last_discovery: Mutex<Option<Instant>>,
    throughput_history: Mutex<HashMap<String, VecDeque<ThroughputSample>>>,
}

#[derive(Clone, Copy, Debug)]
struct ThroughputSample {
    started: Instant,
    finished: Instant,
    tokens_per_second: f32,
}

const MAX_THROUGHPUT_SAMPLES_PER_HOST: usize = 64;

impl FleetService {
    pub fn new(config: FleetConfig, config_dir: PathBuf) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.request_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| format!("failed to create peer HTTP client: {error}"))?;
        let control_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.request_timeout_ms))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| format!("failed to create peer control client: {error}"))?;
        let discovered_hosts = discovery::load(&config_dir)?;
        let hosts = merged_host_configs(&config.hosts, &discovered_hosts)
            .iter()
            .map(|host| initial_host_status(host, &config.local_host_id))
            .collect();

        let opencode = OpenCodeStatus {
            state: if config.opencode.enabled {
                OpenCodeSyncState::Pending
            } else {
                OpenCodeSyncState::Disabled
            },
            selected_model: config.opencode.selected_model.clone(),
            ..OpenCodeStatus::default()
        };
        let hermes = HermesStatus {
            state: if config.hermes.enabled {
                HermesSyncState::Pending
            } else {
                HermesSyncState::Disabled
            },
            selected_model: config.hermes.selected_model.clone(),
            ..HermesStatus::default()
        };
        let hermes_cli = HermesStatus {
            state: if config.hermes.enabled {
                HermesSyncState::Pending
            } else {
                HermesSyncState::Disabled
            },
            selected_model: config.hermes.selected_cli_model.clone(),
            ..HermesStatus::default()
        };

        Ok(Self {
            snapshot: RwLock::new(FleetSnapshot {
                local_host_id: config.local_host_id.clone(),
                config_path: config_dir.join(CONFIG_FILE_NAME).display().to_string(),
                proxy_endpoint: format!("http://{}", config.fleet_proxy.listen_address),
                refreshed_at_ms: now_ms(),
                peer_api: PeerApiStatus::default(),
                hosts,
                opencode,
                hermes,
                hermes_cli,
                codex: HarnessStatus::default(),
                claude_code: HarnessStatus::default(),
                pi: HarnessStatus::default(),
                copilot: HarnessStatus::default(),
                vscode: HarnessStatus::default(),
            }),
            config,
            client,
            control_client,
            refresh_lock: tokio::sync::Mutex::new(()),
            discovered_hosts: RwLock::new(discovered_hosts),
            discovered_hosts_path: discovery::path(&config_dir),
            last_discovery: Mutex::new(None),
            throughput_history: Mutex::new(HashMap::new()),
        })
    }

    pub fn snapshot(&self) -> FleetSnapshot {
        self.snapshot
            .read()
            .expect("fleet snapshot poisoned")
            .clone()
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.config.poll_interval_seconds.max(1))
    }

    pub fn adaptive_poll_interval(&self, control_surface_visible: bool) -> Duration {
        let snapshot = self.snapshot();
        let fleet_active = snapshot
            .hosts
            .iter()
            .any(|host| host.loaded_model_id.is_some() || host.active_requests > 0);
        adaptive_poll_interval(self.poll_interval(), fleet_active, control_surface_visible)
    }

    pub fn local_runtime_active(&self) -> bool {
        let snapshot = self.snapshot();
        snapshot
            .hosts
            .iter()
            .find(|host| host.id == snapshot.local_host_id)
            .is_some_and(|host| host.loaded_model_id.is_some() || host.active_requests > 0)
    }

    pub fn peer_api_port(&self) -> u16 {
        self.config.peer_api_port
    }

    pub fn local_host_id(&self) -> &str {
        &self.config.local_host_id
    }

    pub fn is_local_host(&self, host_id: &str) -> bool {
        host_id == self.config.local_host_id
    }

    pub fn peer_control_endpoint(&self, host_id: &str, action: &str) -> Result<String, String> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
        Ok(format!(
            "http://{}:{}/api/v1/control/{action}",
            host.address, self.config.peer_api_port
        ))
    }

    pub fn peer_proxy_endpoint(
        &self,
        host_id: &str,
        path_and_query: &str,
    ) -> Result<String, String> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
        Ok(format!(
            "http://{}:{}/api/v1/proxy/{}",
            host.address, self.config.peer_api_port, path_and_query
        ))
    }

    pub fn peer_comfy_endpoint(
        &self,
        host_id: &str,
        model_id: &str,
        path_and_query: &str,
    ) -> Result<String, String> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
        Ok(format!(
            "http://{}:{}/api/v1/comfy/{}/{}",
            host.address,
            self.config.peer_api_port,
            urlencoding::encode(model_id),
            path_and_query.trim_start_matches('/')
        ))
    }

    pub fn peer_harness_endpoint(&self, host_id: &str, harness: &str) -> Result<String, String> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
        Ok(format!(
            "http://{}:{}/api/v1/harness/{harness}/deliver",
            host.address, self.config.peer_api_port
        ))
    }

    fn peer_harness_archive_endpoint(
        &self,
        host_id: &str,
        harness: &str,
    ) -> Result<String, String> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
        Ok(format!(
            "http://{}:{}/api/v1/harness/{harness}/session/archive",
            host.address, self.config.peer_api_port
        ))
    }

    fn peer_harness_setup_endpoint(&self, host_id: &str, path: &str) -> Result<String, String> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
        Ok(format!(
            "http://{}:{}/api/v1/{path}",
            host.address, self.config.peer_api_port
        ))
    }

    pub fn local_llama_swap_endpoint(&self, path_and_query: &str) -> String {
        format!(
            "http://{}/{}",
            self.config.llama_swap.listen_address, path_and_query
        )
    }

    pub fn proxy_listen_address(&self) -> &str {
        &self.config.fleet_proxy.listen_address
    }

    pub async fn request_peer_load(
        &self,
        host_id: &str,
        request: &LoadModelRequest,
    ) -> Result<ControlOutcome, String> {
        self.send_peer_control(host_id, "load", request, PEER_LOAD_TIMEOUT)
            .await
    }

    pub async fn request_peer_unload(
        &self,
        host_id: &str,
        request: &UnloadModelsRequest,
    ) -> Result<ControlOutcome, String> {
        self.send_peer_control(host_id, "unload", request, PEER_UNLOAD_TIMEOUT)
            .await
    }

    pub async fn request_peer_harness_statuses(
        &self,
        host_id: &str,
    ) -> Result<Vec<HarnessSetupStatus>, String> {
        let endpoint = self.peer_harness_setup_endpoint(host_id, "harnesses")?;
        let response = self
            .control_client
            .get(endpoint)
            .timeout(PEER_UNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("failed to inspect harnesses on {host_id}: {error}"))?;
        parse_peer_json(response, "harness status").await
    }

    pub async fn request_peer_harness_configure(
        &self,
        host_id: &str,
        request: &HarnessSetupRequest,
    ) -> Result<HarnessSetupStatus, String> {
        let endpoint = self.peer_harness_setup_endpoint(host_id, "harness/configure")?;
        let response = self
            .control_client
            .post(endpoint)
            .json(request)
            .timeout(PEER_HARNESS_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("failed to configure harness on {host_id}: {error}"))?;
        parse_peer_json(response, "harness configuration").await
    }

    pub async fn request_peer_opencode_sessions(
        &self,
        host_id: &str,
    ) -> Result<Vec<crate::opencode::OpenCodeSessionInfo>, String> {
        let endpoint = self.peer_harness_setup_endpoint(host_id, "harness/opencode/sessions")?;
        let response = self
            .control_client
            .get(endpoint)
            .timeout(PEER_UNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|error| {
                format!("failed to inspect OpenCode sessions on {host_id}: {error}")
            })?;
        parse_peer_json(response, "OpenCode session inventory").await
    }

    pub async fn request_peer_hermes_delivery(
        &self,
        host_id: &str,
        request: &HarnessDeliveryRequest,
    ) -> Result<HarnessDeliveryResponse, String> {
        self.request_peer_harness_delivery(host_id, "hermes", request)
            .await
    }

    pub async fn request_peer_harness_delivery(
        &self,
        host_id: &str,
        harness: &str,
        request: &HarnessDeliveryRequest,
    ) -> Result<HarnessDeliveryResponse, String> {
        let endpoint = self.peer_harness_endpoint(host_id, harness)?;
        let display_name = match harness {
            "hermes" => "Hermes",
            "opencode" => "OpenCode",
            "pi" => "Pi",
            _ => return Err(format!("unsupported remote harness: {harness}")),
        };
        let operation = async {
            let response = self
                .control_client
                .post(endpoint)
                .json(request)
                .send()
                .await
                .map_err(|error| {
                    format!("failed to contact {display_name} on {host_id}: {error}")
                })?;
            let status = response.status();
            let bytes = response.bytes().await.map_err(|error| {
                format!("failed to read {display_name} response from {host_id}: {error}")
            })?;
            if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
                return Err(format!(
                    "{display_name} response from {host_id} was too large"
                ));
            }
            if !status.is_success() {
                let error = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|body| {
                        body.get("error")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| {
                        format!("{display_name} on {host_id} returned HTTP {status}")
                    });
                return Err(error);
            }
            serde_json::from_slice(&bytes).map_err(|error| {
                format!("{display_name} on {host_id} returned invalid JSON: {error}")
            })
        };
        tokio::time::timeout(PEER_HARNESS_TIMEOUT, operation)
            .await
            .map_err(|_| format!("timed out waiting for {display_name} on {host_id}"))?
    }

    pub async fn request_peer_harness_session_archive(
        &self,
        host_id: &str,
        harness: &str,
        request: &HarnessSessionArchiveRequest,
    ) -> Result<(), String> {
        let endpoint = self.peer_harness_archive_endpoint(host_id, harness)?;
        let response = self
            .control_client
            .post(endpoint)
            .json(request)
            .timeout(PEER_UNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("failed to update {harness} session on {host_id}: {error}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let payload: serde_json::Value = response.json().await.unwrap_or_default();
        Err(payload
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!("{harness} session update on {host_id} returned HTTP {status}")
            }))
    }

    async fn send_peer_control<T: serde::Serialize + ?Sized>(
        &self,
        host_id: &str,
        action: &str,
        request: &T,
        timeout: Duration,
    ) -> Result<ControlOutcome, String> {
        let endpoint = self.peer_control_endpoint(host_id, action)?;
        let operation = async {
            let response = self
                .control_client
                .post(endpoint)
                .json(request)
                .send()
                .await
                .map_err(|error| format!("failed to contact {host_id}: {error}"))?;
            parse_control_response(response).await
        };
        tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| {
                format!(
                    "timed out waiting for {host_id} to {action} after {} seconds",
                    timeout.as_secs()
                )
            })?
    }

    pub fn local_peer_status(&self) -> PeerStatusResponse {
        let snapshot = self.snapshot();
        let local = snapshot
            .hosts
            .iter()
            .find(|host| host.id == snapshot.local_host_id);

        PeerStatusResponse {
            protocol: Some(crate::discovery::PEER_PROTOCOL.to_owned()),
            host_id: snapshot.local_host_id,
            display_name: local.map(|host| host.display_name.clone()),
            hardware: local.map(|host| host.hardware.clone()),
            models: local.map(|host| host.models.clone()).unwrap_or_default(),
            loaded_model_id: local.and_then(|host| host.loaded_model_id.clone()),
            active_requests: local.map(|host| host.active_requests).unwrap_or_default(),
            memory_used_bytes: local.and_then(|host| host.memory_used_bytes),
            memory_total_bytes: local.and_then(|host| host.memory_total_bytes),
            memory_kind: local.and_then(|host| host.memory_kind.clone()),
            tokens_per_second: local.and_then(|host| host.tokens_per_second),
            aggregate_tokens_per_second: local.and_then(|host| host.aggregate_tokens_per_second),
            throughput_concurrency: local
                .map(|host| host.throughput_concurrency)
                .unwrap_or_default(),
            llama_swap: local
                .map(|host| host.llama_swap.clone())
                .unwrap_or_default(),
            channel_gateway: local.and_then(|host| host.channel_gateway.clone()),
        }
    }

    pub fn update_local_runtime(
        &self,
        llama_swap: LlamaSwapStatus,
        models: Vec<ModelProfile>,
        loaded_model_id: Option<String>,
        active_requests: u32,
    ) {
        let model_changed = {
            let mut snapshot = self.snapshot.write().expect("fleet snapshot poisoned");
            let local_host_id = snapshot.local_host_id.clone();
            let mut model_changed = false;
            if let Some(local) = snapshot
                .hosts
                .iter_mut()
                .find(|host| host.id == local_host_id)
            {
                model_changed = local.loaded_model_id != loaded_model_id;
                if model_changed {
                    local.tokens_per_second = None;
                    local.aggregate_tokens_per_second = None;
                    local.throughput_concurrency = 0;
                }
                local.llama_swap = llama_swap;
                local.models = models;
                local.loaded_model_id = loaded_model_id;
                local.active_requests = active_requests;
                local.last_seen_at_ms = Some(now_ms());
            }
            snapshot.refreshed_at_ms = now_ms();
            model_changed
        };
        if model_changed {
            self.throughput_history
                .lock()
                .expect("throughput history poisoned")
                .remove(self.local_host_id());
        }
    }

    pub fn update_llama_swap_status(&self, llama_swap: LlamaSwapStatus) {
        let mut snapshot = self.snapshot.write().expect("fleet snapshot poisoned");
        let local_host_id = snapshot.local_host_id.clone();
        if let Some(local) = snapshot
            .hosts
            .iter_mut()
            .find(|host| host.id == local_host_id)
        {
            local.llama_swap = llama_swap;
            local.last_seen_at_ms = Some(now_ms());
        }
        snapshot.refreshed_at_ms = now_ms();
    }

    pub fn update_active_requests(&self, active_requests: u32) {
        let mut snapshot = self.snapshot.write().expect("fleet snapshot poisoned");
        let local_host_id = snapshot.local_host_id.clone();
        if let Some(local) = snapshot
            .hosts
            .iter_mut()
            .find(|host| host.id == local_host_id)
        {
            local.active_requests = active_requests;
        }
        snapshot.refreshed_at_ms = now_ms();
    }

    pub fn record_generation_throughput(
        &self,
        host_id: &str,
        started: Instant,
        finished: Instant,
        tokens_per_second: f32,
    ) {
        if !tokens_per_second.is_finite() || tokens_per_second <= 0.0 {
            return;
        }
        let sample = ThroughputSample {
            started,
            finished,
            tokens_per_second,
        };
        let (aggregate, concurrency) = {
            let mut histories = self
                .throughput_history
                .lock()
                .expect("throughput history poisoned");
            let history = histories.entry(host_id.to_owned()).or_default();
            history.push_back(sample);
            while history.len() > MAX_THROUGHPUT_SAMPLES_PER_HOST {
                history.pop_front();
            }
            aggregate_overlapping_throughput(history, sample)
        };
        let mut snapshot = self.snapshot.write().expect("fleet snapshot poisoned");
        if let Some(host) = snapshot.hosts.iter_mut().find(|host| host.id == host_id) {
            host.tokens_per_second = Some(tokens_per_second);
            host.aggregate_tokens_per_second = Some(aggregate);
            host.throughput_concurrency = concurrency;
        }
        snapshot.refreshed_at_ms = now_ms();
    }

    pub fn update_local_memory(&self, used_bytes: u64, total_bytes: u64, kind: String) {
        let mut snapshot = self.snapshot.write().expect("fleet snapshot poisoned");
        let local_host_id = snapshot.local_host_id.clone();
        if let Some(local) = snapshot
            .hosts
            .iter_mut()
            .find(|host| host.id == local_host_id)
        {
            local.memory_used_bytes = Some(used_bytes);
            local.memory_total_bytes = Some(total_bytes);
            local.memory_kind = Some(kind);
        }
        snapshot.refreshed_at_ms = now_ms();
    }

    pub fn update_opencode_status(&self, status: OpenCodeStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .opencode = status;
    }

    pub fn update_hermes_status(&self, status: HermesStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .hermes = status;
    }

    pub fn update_hermes_cli_status(&self, status: HermesStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .hermes_cli = status;
    }

    pub fn update_codex_status(&self, status: HarnessStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .codex = status;
    }

    pub fn update_claude_code_status(&self, status: HarnessStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .claude_code = status;
    }

    pub fn update_pi_status(&self, status: HarnessStatus) {
        self.snapshot.write().expect("fleet snapshot poisoned").pi = status;
    }

    pub fn update_copilot_status(&self, status: HarnessStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .copilot = status;
    }

    pub fn update_vscode_status(&self, status: HarnessStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .vscode = status;
    }

    pub fn update_peer_api_status(&self, status: PeerApiStatus) {
        self.snapshot
            .write()
            .expect("fleet snapshot poisoned")
            .peer_api = status;
    }

    pub fn update_channel_gateway_status(
        &self,
        status: Option<crate::domain::GatewayRuntimeStatus>,
    ) {
        let mut snapshot = self.snapshot.write().expect("fleet snapshot poisoned");
        let local_host_id = snapshot.local_host_id.clone();
        if let Some(local) = snapshot
            .hosts
            .iter_mut()
            .find(|host| host.id == local_host_id)
        {
            local.channel_gateway = status;
        }
        snapshot.refreshed_at_ms = now_ms();
    }

    pub async fn refresh(&self) -> FleetSnapshot {
        let _refresh_guard = self.refresh_lock.lock().await;
        self.refresh_discovery().await;
        let previous = self.snapshot();
        let hosts = self.host_configs();
        let peer_statuses = futures_util::future::join_all(
            hosts
                .iter()
                .filter(|host| host.id != self.config.local_host_id)
                .map(|host| self.poll_peer(host, &previous)),
        )
        .await;

        let mut latest = self.snapshot.write().expect("fleet snapshot poisoned");
        merge_peer_refresh(
            &hosts,
            &self.config.local_host_id,
            &previous,
            &mut latest,
            peer_statuses,
        );
        latest.clone()
    }

    fn host_configs(&self) -> Vec<HostConfig> {
        merged_host_configs(
            &self.config.hosts,
            &self
                .discovered_hosts
                .read()
                .expect("discovered hosts poisoned"),
        )
    }

    fn host_config(&self, host_id: &str) -> Option<HostConfig> {
        self.host_configs()
            .into_iter()
            .find(|host| host.id == host_id)
    }

    async fn refresh_discovery(&self) {
        let should_scan = {
            let mut last = self.last_discovery.lock().expect("last discovery poisoned");
            if last.is_some_and(|instant| instant.elapsed() < DISCOVERY_INTERVAL) {
                false
            } else {
                *last = Some(Instant::now());
                true
            }
        };
        if !should_scan {
            return;
        }
        let discovered = match discovery::scan(
            &self.client,
            self.config.peer_api_port,
            &self.config.local_host_id,
        )
        .await
        {
            Ok(discovered) => discovered,
            Err(error) => {
                eprintln!("Agent Relay discovery unavailable: {error}");
                return;
            }
        };
        if discovered.is_empty() {
            return;
        }
        let manual_ids = self
            .config
            .hosts
            .iter()
            .map(|host| host.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut stored = self
            .discovered_hosts
            .write()
            .expect("discovered hosts poisoned");
        let mut changed = false;
        for host in discovered {
            if manual_ids.contains(host.id.as_str()) {
                continue;
            }
            match stored.iter_mut().find(|existing| existing.id == host.id) {
                Some(existing) if *existing != host => {
                    *existing = host;
                    changed = true;
                }
                None => {
                    stored.push(host);
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            stored.sort_by(|left, right| left.id.cmp(&right.id));
            if let Err(error) = discovery::persist(&self.discovered_hosts_path, &stored) {
                eprintln!("failed to persist discovered Agent Relay hosts: {error}");
            }
        }
    }

    async fn poll_peer(&self, host: &HostConfig, previous: &FleetSnapshot) -> HostStatus {
        let endpoint = format!(
            "http://{}:{}/api/v1/status",
            host.address, self.config.peer_api_port
        );
        let cached = previous.hosts.iter().find(|status| status.id == host.id);

        match self.client.get(&endpoint).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<PeerStatusResponse>().await {
                    Ok(peer) if peer.host_id == host.id => online_status(host, peer),
                    Ok(peer) => offline_status(
                        host,
                        cached,
                        format!("peer identified itself as {}", peer.host_id),
                    ),
                    Err(error) => {
                        offline_status(host, cached, format!("invalid peer response: {error}"))
                    }
                },
                Err(error) => {
                    offline_status(host, cached, format!("peer returned an error: {error}"))
                }
            },
            Err(error) => offline_status(host, cached, format!("peer unavailable: {error}")),
        }
    }
}

fn adaptive_poll_interval(
    active_interval: Duration,
    fleet_active: bool,
    control_surface_visible: bool,
) -> Duration {
    if fleet_active || control_surface_visible {
        active_interval
    } else {
        active_interval.max(IDLE_POLL_INTERVAL)
    }
}

async fn parse_control_response(response: reqwest::Response) -> Result<ControlOutcome, String> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read peer control response: {error}"))?;
    if body.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(format!(
            "peer control response exceeded {MAX_CONTROL_RESPONSE_BYTES} bytes"
        ));
    }
    if status.is_success() || status == reqwest::StatusCode::CONFLICT {
        return serde_json::from_slice(&body)
            .map_err(|error| format!("invalid peer control response: {error}"));
    }
    let detail = String::from_utf8_lossy(&body);
    Err(format!("peer control request failed ({status}): {detail}"))
}

async fn parse_peer_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, String> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read peer {operation} response: {error}"))?;
    if body.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(format!(
            "peer {operation} response exceeded {MAX_CONTROL_RESPONSE_BYTES} bytes"
        ));
    }
    if !status.is_success() {
        let detail = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        return Err(format!("peer {operation} failed ({status}): {detail}"));
    }
    serde_json::from_slice(&body)
        .map_err(|error| format!("invalid peer {operation} response: {error}"))
}

pub type SharedFleetService = Arc<FleetService>;

fn initial_host_status(host: &HostConfig, local_host_id: &str) -> HostStatus {
    HostStatus {
        id: host.id.clone(),
        display_name: host.display_name.clone(),
        address: host.address.clone(),
        hardware: host.hardware.clone(),
        connection: if host.id == local_host_id {
            ConnectionState::Local
        } else {
            ConnectionState::Offline
        },
        models: Vec::new(),
        loaded_model_id: None,
        active_requests: 0,
        memory_used_bytes: None,
        memory_total_bytes: None,
        memory_kind: None,
        tokens_per_second: None,
        aggregate_tokens_per_second: None,
        throughput_concurrency: 0,
        last_seen_at_ms: (host.id == local_host_id).then(now_ms),
        error: None,
        llama_swap: LlamaSwapStatus::default(),
        channel_gateway: None,
    }
}

fn online_status(host: &HostConfig, peer: PeerStatusResponse) -> HostStatus {
    HostStatus {
        id: host.id.clone(),
        display_name: host.display_name.clone(),
        address: host.address.clone(),
        hardware: host.hardware.clone(),
        connection: ConnectionState::Online,
        models: peer.models,
        loaded_model_id: peer.loaded_model_id,
        active_requests: peer.active_requests,
        memory_used_bytes: peer.memory_used_bytes,
        memory_total_bytes: peer.memory_total_bytes,
        memory_kind: peer.memory_kind,
        tokens_per_second: peer.tokens_per_second,
        aggregate_tokens_per_second: peer.aggregate_tokens_per_second,
        throughput_concurrency: peer.throughput_concurrency,
        last_seen_at_ms: Some(now_ms()),
        error: None,
        llama_swap: peer.llama_swap,
        channel_gateway: peer.channel_gateway,
    }
}

fn offline_status(host: &HostConfig, cached: Option<&HostStatus>, error: String) -> HostStatus {
    let mut status = cached
        .cloned()
        .unwrap_or_else(|| initial_host_status(host, ""));
    status.connection = ConnectionState::Offline;
    status.error = Some(error);
    status
}

fn merge_peer_refresh(
    hosts: &[HostConfig],
    local_host_id: &str,
    baseline: &FleetSnapshot,
    latest: &mut FleetSnapshot,
    peer_statuses: Vec<HostStatus>,
) {
    let mut current = std::mem::take(&mut latest.hosts)
        .into_iter()
        .map(|host| (host.id.clone(), host))
        .collect::<HashMap<_, _>>();
    let mut peers = peer_statuses
        .into_iter()
        .map(|host| (host.id.clone(), host))
        .collect::<HashMap<_, _>>();

    latest.hosts = hosts
        .iter()
        .map(|host| {
            if host.id == local_host_id {
                let mut local = current
                    .remove(&host.id)
                    .unwrap_or_else(|| initial_host_status(host, local_host_id));
                local.connection = ConnectionState::Local;
                local.error = None;
                local.last_seen_at_ms = Some(now_ms());
                local
            } else {
                let mut refreshed = peers
                    .remove(&host.id)
                    .or_else(|| current.remove(&host.id))
                    .unwrap_or_else(|| initial_host_status(host, local_host_id));
                let baseline_host = baseline.hosts.iter().find(|current| current.id == host.id);
                let latest_host = current.get(&host.id);
                if peer_metrics_changed_while_refreshing(baseline_host, latest_host) {
                    if let Some(latest_host) = latest_host {
                        refreshed.tokens_per_second = latest_host.tokens_per_second;
                        refreshed.aggregate_tokens_per_second =
                            latest_host.aggregate_tokens_per_second;
                        refreshed.throughput_concurrency = latest_host.throughput_concurrency;
                    }
                }
                refreshed
            }
        })
        .collect();
    latest.refreshed_at_ms = now_ms();
}

fn merged_host_configs(manual: &[HostConfig], discovered: &[HostConfig]) -> Vec<HostConfig> {
    let mut hosts = manual.to_vec();
    let manual_ids = manual
        .iter()
        .map(|host| host.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    hosts.extend(
        discovered
            .iter()
            .filter(|host| !manual_ids.contains(host.id.as_str()))
            .cloned(),
    );
    hosts
}

fn peer_metrics_changed_while_refreshing(
    baseline: Option<&HostStatus>,
    latest: Option<&HostStatus>,
) -> bool {
    match (baseline, latest) {
        (Some(baseline), Some(latest)) => {
            baseline.tokens_per_second != latest.tokens_per_second
                || baseline.aggregate_tokens_per_second != latest.aggregate_tokens_per_second
                || baseline.throughput_concurrency != latest.throughput_concurrency
        }
        (None, Some(latest)) => {
            latest.tokens_per_second.is_some()
                || latest.aggregate_tokens_per_second.is_some()
                || latest.throughput_concurrency != 0
        }
        _ => false,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn aggregate_overlapping_throughput(
    history: &VecDeque<ThroughputSample>,
    current: ThroughputSample,
) -> (f32, u32) {
    let overlapping = history
        .iter()
        .filter(|sample| sample.started < current.finished && sample.finished > current.started);
    overlapping.fold((0.0, 0), |(total, count), sample| {
        (total + sample.tokens_per_second, count + 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ModelProfile, ProfileCapability};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn host() -> HostConfig {
        HostConfig {
            id: "air-m4".into(),
            display_name: "Air-M4".into(),
            address: "air-m4".into(),
            hardware: "24 GB unified memory".into(),
        }
    }

    #[test]
    fn aggregates_only_requests_with_overlapping_generation_windows() {
        let base = Instant::now();
        let first = ThroughputSample {
            started: base,
            finished: base + Duration::from_secs(10),
            tokens_per_second: 20.0,
        };
        let separate = ThroughputSample {
            started: base + Duration::from_secs(20),
            finished: base + Duration::from_secs(25),
            tokens_per_second: 40.0,
        };
        let current = ThroughputSample {
            started: base + Duration::from_secs(5),
            finished: base + Duration::from_secs(12),
            tokens_per_second: 15.0,
        };
        let history = VecDeque::from([first, separate, current]);

        assert_eq!(
            aggregate_overlapping_throughput(&history, current),
            (35.0, 2)
        );
    }

    #[test]
    fn offline_status_preserves_cached_catalog() {
        let mut cached = initial_host_status(&host(), "");
        cached.models.push(ModelProfile {
            id: "qwen-mlx".into(),
            display_name: "Qwen MLX".into(),
            runtime: "mlx".into(),
            kind: Default::default(),
            capabilities: vec![ProfileCapability::Chat],
            lifecycle_adapter: "llama_swap".into(),
            resource_pool: "default".into(),
            context_length: None,
            inference_controls: Default::default(),
        });
        cached.last_seen_at_ms = Some(42);

        let offline = offline_status(&host(), Some(&cached), "asleep".into());
        assert_eq!(offline.models, cached.models);
        assert_eq!(offline.last_seen_at_ms, Some(42));
        assert_eq!(offline.connection, ConnectionState::Offline);
    }

    #[test]
    fn peer_refresh_merges_into_the_latest_local_and_connector_state() {
        let mut config = FleetConfig::defaults("GPU Workstation");
        config.hosts.push(host());
        let service = FleetService::new(config.clone(), PathBuf::from("config")).expect("service");
        let baseline = service.snapshot();
        let mut latest = baseline.clone();
        let local = latest
            .hosts
            .iter_mut()
            .find(|host| host.id == "gpu-workstation")
            .expect("local host");
        local.loaded_model_id = Some("new-local-model".into());
        local.active_requests = 2;
        local.tokens_per_second = Some(42.0);
        latest.hermes.selected_model = Some("gpu-workstation/new-local-model".into());

        let mut peer = initial_host_status(&host(), "gpu-workstation");
        peer.connection = ConnectionState::Online;
        peer.loaded_model_id = Some("remote-model".into());
        let remote = latest
            .hosts
            .iter_mut()
            .find(|host| host.id == "air-m4")
            .expect("remote host");
        remote.tokens_per_second = Some(99.0);
        remote.aggregate_tokens_per_second = Some(101.0);
        remote.throughput_concurrency = 3;

        merge_peer_refresh(
            &config.hosts,
            &config.local_host_id,
            &baseline,
            &mut latest,
            vec![peer],
        );

        let local = latest
            .hosts
            .iter()
            .find(|host| host.id == "gpu-workstation")
            .expect("local host");
        assert_eq!(local.loaded_model_id.as_deref(), Some("new-local-model"));
        assert_eq!(local.active_requests, 2);
        assert_eq!(local.tokens_per_second, Some(42.0));
        assert_eq!(
            latest.hermes.selected_model.as_deref(),
            Some("gpu-workstation/new-local-model")
        );
        assert_eq!(
            latest
                .hosts
                .iter()
                .find(|host| host.id == "air-m4")
                .and_then(|host| host.loaded_model_id.as_deref()),
            Some("remote-model")
        );
        let remote = latest
            .hosts
            .iter()
            .find(|host| host.id == "air-m4")
            .expect("remote host");
        assert_eq!(remote.tokens_per_second, Some(99.0));
        assert_eq!(remote.aggregate_tokens_per_second, Some(101.0));
        assert_eq!(remote.throughput_concurrency, 3);
    }

    #[test]
    fn peer_control_requests_have_action_specific_deadlines() {
        assert_eq!(PEER_LOAD_TIMEOUT, Duration::from_secs(600));
        assert_eq!(PEER_UNLOAD_TIMEOUT, Duration::from_secs(30));

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind stalled peer");
        let port = listener.local_addr().expect("peer address").port();
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept control request");
            let mut request = [0_u8; 4096];
            let _ = connection.read(&mut request).expect("read control request");
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\n\r\n")
                .expect("write response headers");
            connection.flush().expect("flush response headers");
            thread::sleep(Duration::from_millis(250));
        });

        let mut config = FleetConfig::defaults("GPU Workstation");
        config.hosts.push(host());
        config.peer_api_port = port;
        config
            .hosts
            .iter_mut()
            .find(|host| host.id == "air-m4")
            .expect("remote host")
            .address = "127.0.0.1".into();
        let service = FleetService::new(config, PathBuf::from("config")).expect("service");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let error = runtime
            .block_on(service.send_peer_control(
                "air-m4",
                "unload",
                &UnloadModelsRequest { force: false },
                Duration::from_millis(25),
            ))
            .expect_err("stalled peer must time out");
        assert!(
            error.contains("timed out waiting for air-m4 to unload"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn adaptive_polling_stays_responsive_only_when_needed() {
        let active = Duration::from_secs(5);
        assert_eq!(adaptive_poll_interval(active, true, false), active);
        assert_eq!(adaptive_poll_interval(active, false, true), active);
        assert_eq!(
            adaptive_poll_interval(active, false, false),
            Duration::from_secs(30)
        );
        assert_eq!(
            adaptive_poll_interval(Duration::from_secs(60), false, false),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn comfy_peer_endpoint_encodes_the_profile_segment() {
        let mut config = FleetConfig::defaults("GPU Workstation");
        config.hosts.push(host());
        let service = FleetService::new(config, PathBuf::from("config")).expect("service");
        assert_eq!(
            service
                .peer_comfy_endpoint("air-m4", "Comfy Workflow", "history/job-1?x=1")
                .expect("endpoint"),
            "http://air-m4:38473/api/v1/comfy/Comfy%20Workflow/history/job-1?x=1"
        );
    }
}
