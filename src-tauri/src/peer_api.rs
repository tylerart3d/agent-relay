use std::{env, net::Ipv4Addr, path::PathBuf, process::Command, time::Duration};

use axum::{
    body::Body,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    domain::{
        ControlOutcome, ControlState, LoadModelRequest, PeerApiState, PeerApiStatus,
        PeerStatusResponse, ProfileCapability, UnloadModelsRequest, WorkloadKind,
    },
    fleet::SharedFleetService,
    fleet_proxy,
    harness_messages::{HarnessDeliveryRequest, HarnessSessionArchiveRequest},
    harness_setup::{self, HarnessSetupRequest},
    hermes::SharedHermesIntegration,
    llama_swap::SharedLlamaSwapSupervisor,
    local_harness::SharedLocalHarnessIntegrations,
    opencode::SharedOpenCodeIntegration,
    pi_runner::SharedPiRunner,
    telemetry::SharedTelemetry,
};

const RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct PeerServerState {
    fleet: SharedFleetService,
    llama_swap: SharedLlamaSwapSupervisor,
    relay_client: reqwest::Client,
    hermes: SharedHermesIntegration,
    opencode: SharedOpenCodeIntegration,
    pi: SharedPiRunner,
    harnesses: SharedLocalHarnessIntegrations,
    telemetry: SharedTelemetry,
}

#[derive(Clone)]
pub(crate) struct PeerIntegrations {
    pub(crate) hermes: SharedHermesIntegration,
    pub(crate) opencode: SharedOpenCodeIntegration,
    pub(crate) pi: SharedPiRunner,
    pub(crate) harnesses: SharedLocalHarnessIntegrations,
}

pub fn tailscale_ipv4() -> Result<Ipv4Addr, String> {
    let mut failures = Vec::new();
    for executable in tailscale_candidates() {
        if executable.is_absolute() && !executable.is_file() {
            continue;
        }
        let mut command = Command::new(&executable);
        command.args(["ip", "-4"]).env("TAILSCALE_BE_CLI", "1");

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        match command.output() {
            Ok(output) if output.status.success() => {
                if let Some(address) = parse_tailscale_ipv4(&output.stdout) {
                    return Ok(address);
                }
                failures.push(format!("{} returned no IPv4 address", executable.display()));
            }
            Ok(output) => failures.push(format!(
                "{} failed: {}",
                executable.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => failures.push(format!("{}: {error}", executable.display())),
        }
    }

    Err(format!(
        "could not obtain a Tailscale IPv4 address{}",
        if failures.is_empty() {
            String::new()
        } else {
            format!(": {}", failures.join("; "))
        }
    ))
}

fn parse_tailscale_ipv4(output: &[u8]) -> Option<Ipv4Addr> {
    String::from_utf8_lossy(output)
        .lines()
        .find_map(|line| line.trim().parse::<Ipv4Addr>().ok())
}

pub(crate) fn tailscale_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("TAILSCALE_CLI_PATH") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(PathBuf::from("tailscale"));

    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/usr/local/bin/tailscale"),
        PathBuf::from("/opt/homebrew/bin/tailscale"),
        PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
    ]);
    #[cfg(windows)]
    if let Some(program_files) = env::var_os("ProgramFiles") {
        candidates.push(
            PathBuf::from(program_files)
                .join("Tailscale")
                .join("tailscale.exe"),
        );
    }
    candidates
}

pub async fn serve(
    fleet: SharedFleetService,
    llama_swap: SharedLlamaSwapSupervisor,
    integrations: PeerIntegrations,
    telemetry: SharedTelemetry,
    address: Ipv4Addr,
) -> Result<(), String> {
    let socket = (address, fleet.peer_api_port());
    let relay_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(1_500))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to create peer relay HTTP client: {error}"))?;
    let state = PeerServerState {
        fleet: fleet.clone(),
        llama_swap,
        relay_client,
        hermes: integrations.hermes,
        opencode: integrations.opencode,
        pi: integrations.pi,
        harnesses: integrations.harnesses,
        telemetry,
    };
    let app = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/control/load", post(load_model))
        .route("/api/v1/control/availability", post(set_availability))
        .route("/api/v1/control/prepare-auto", post(prepare_auto))
        .route("/api/v1/control/reserve-auto", post(reserve_auto))
        .route("/api/v1/control/release-auto", post(release_auto))
        .route("/api/v1/control/reload", post(reload_model))
        .route("/api/v1/control/unload", post(unload_models))
        .route("/api/v1/harnesses", get(harness_statuses))
        .route("/api/v1/harness/configure", post(configure_harness))
        .route("/api/v1/harness/hermes/deliver", post(deliver_hermes))
        .route("/api/v1/harness/opencode/sessions", get(opencode_sessions))
        .route("/api/v1/harness/opencode/deliver", post(deliver_opencode))
        .route("/api/v1/harness/pi/deliver", post(deliver_pi))
        .route(
            "/api/v1/harness/{harness}/session/archive",
            post(set_harness_session_archived),
        )
        .route("/api/v1/comfy/{model_id}/{*path}", any(proxy_comfy_request))
        .route(
            "/api/v1/worker/{model_id}/{*path}",
            any(proxy_worker_request),
        )
        .route("/api/v1/proxy/{*path}", any(proxy_request))
        .route("/metrics", get(prometheus_metrics))
        .merge(crate::voice::peer_router(
            fleet.clone(),
            state.llama_swap.clone(),
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(socket)
        .await
        .map_err(|error| {
            format!(
                "failed to bind peer API to {}:{}: {error}",
                socket.0, socket.1
            )
        })?;
    fleet.update_peer_api_status(PeerApiStatus {
        state: PeerApiState::Listening,
        address: Some(format!("{}:{}", socket.0, socket.1)),
        error: None,
    });

    axum::serve(listener, app)
        .await
        .map_err(|error| format!("peer API stopped: {error}"))
}

pub async fn supervise(
    fleet: SharedFleetService,
    llama_swap: SharedLlamaSwapSupervisor,
    integrations: PeerIntegrations,
    telemetry: SharedTelemetry,
) {
    loop {
        let result = match tailscale_ipv4() {
            Ok(address) => {
                serve(
                    fleet.clone(),
                    llama_swap.clone(),
                    integrations.clone(),
                    telemetry.clone(),
                    address,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let error = result
            .err()
            .unwrap_or_else(|| "peer API stopped unexpectedly".to_owned());
        fleet.update_peer_api_status(PeerApiStatus {
            state: PeerApiState::Error,
            address: None,
            error: Some(format!("{error}; retrying in 5 seconds")),
        });
        eprintln!("{error}; retrying peer API in 5 seconds");
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

async fn prometheus_metrics(State(state): State<PeerServerState>) -> Response {
    match state.telemetry.prometheus(&state.fleet.snapshot()) {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn proxy_request(
    State(state): State<PeerServerState>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    mut headers: HeaderMap,
    body: Body,
) -> Response {
    let observes_generation = fleet_proxy::is_generation_path(&path);
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    if let Err(error) = state.fleet.require_local_available() {
        return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
    }
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let payload: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let Some(model) = payload.get("model").and_then(serde_json::Value::as_str) else {
        return (StatusCode::BAD_REQUEST, "Missing model").into_response();
    };
    let cpu_embedding = state
        .fleet
        .local_peer_status()
        .models
        .iter()
        .any(|p| p.id == model && p.is_cpu_embedding_service());
    let slot = if observes_generation && !cpu_embedding {
        match headers.get("x-agentrelay-reservation") {
            Some(token) => match token
                .to_str()
                .ok()
                .and_then(|t| state.llama_swap.admissions.claim(t, model).ok())
            {
                Some(lease) => Some(lease),
                None => {
                    return (
                        StatusCode::CONFLICT,
                        "Invalid or expired Auto reservation; prompt not submitted",
                    )
                        .into_response()
                }
            },
            None if headers.contains_key("x-agentrelay-auto") => {
                return (
                    StatusCode::CONFLICT,
                    "Auto inference requires a reservation from this host",
                )
                    .into_response()
            }
            None => Some(state.llama_swap.admissions.track(model)),
        }
    } else {
        None
    };
    let (endpoint, admission) = match state
        .llama_swap
        .prepare_inference(
            model,
            &path_and_query,
            headers.contains_key("x-agentrelay-auto"),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    let mut observer = if observes_generation {
        match fleet_proxy::GenerationObserver::try_new(
            state.fleet.clone(),
            state.fleet.local_host_id().to_owned(),
        ) {
            Ok(observer) => Some(observer),
            Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
        }
    } else {
        None
    };
    if let Some(observer) = &mut observer {
        observer.hold_slot(slot);
    }
    drop(admission);
    headers.remove("x-agentrelay-reservation");
    headers.remove("x-agentrelay-auto");
    fleet_proxy::forward_buffered(
        &state.relay_client,
        method,
        headers,
        endpoint,
        bytes,
        observer,
    )
    .await
}

async fn proxy_comfy_request(
    State(state): State<PeerServerState>,
    Path((model_id, path)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(error) = state.fleet.require_local_available() {
        return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
    }
    if !fleet_proxy::comfy_path_allowed(&path, &method) {
        return (
            StatusCode::NOT_FOUND,
            format!("ComfyUI route is not exposed: {method} /{path}"),
        )
            .into_response();
    }
    let snapshot = state.fleet.snapshot();
    let Some(profile) = snapshot
        .hosts
        .iter()
        .find(|host| host.id == snapshot.local_host_id)
        .and_then(|host| host.models.iter().find(|profile| profile.id == model_id))
    else {
        return (StatusCode::NOT_FOUND, "unknown local workflow profile").into_response();
    };
    if profile.kind != WorkloadKind::Image
        || !profile
            .capabilities
            .contains(&ProfileCapability::WorkflowQueue)
    {
        return (StatusCode::BAD_REQUEST, "profile is not a ComfyUI workflow").into_response();
    }
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let endpoint = match state
        .llama_swap
        .ready_upstream_endpoint(&model_id, &path_and_query)
        .await
    {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => match state.llama_swap.load_model(&model_id, false).await {
            Ok(outcome) if outcome.state == ControlState::Conflict => {
                return (StatusCode::CONFLICT, outcome.message).into_response()
            }
            Ok(_) => match state
                .llama_swap
                .ready_upstream_endpoint(&model_id, &path_and_query)
                .await
            {
                Ok(Some(endpoint)) => endpoint,
                Ok(None) => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "workflow profile did not expose a ready endpoint",
                    )
                        .into_response()
                }
                Err(error) => return (StatusCode::BAD_GATEWAY, error).into_response(),
            },
            Err(error) => return (StatusCode::BAD_GATEWAY, error).into_response(),
        },
        Err(error) => return (StatusCode::BAD_GATEWAY, error).into_response(),
    };
    fleet_proxy::forward_streaming(&state.relay_client, method, headers, endpoint, body, None).await
}

async fn proxy_worker_request(
    State(state): State<PeerServerState>,
    Path((model_id, path)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(error) = state.fleet.require_local_available() {
        return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
    }
    if !fleet_proxy::worker_path_allowed(&path, &method) {
        return (
            StatusCode::NOT_FOUND,
            format!("worker route is not exposed: {method} /{path}"),
        )
            .into_response();
    }
    let snapshot = state.fleet.snapshot();
    let Some(profile) = snapshot
        .hosts
        .iter()
        .find(|host| host.id == snapshot.local_host_id)
        .and_then(|host| host.models.iter().find(|profile| profile.id == model_id))
    else {
        return (StatusCode::NOT_FOUND, "unknown local worker profile").into_response();
    };
    if !profile.is_worker_service() {
        return (StatusCode::BAD_REQUEST, "profile is not a worker service").into_response();
    }
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let endpoint = match state
        .llama_swap
        .ready_upstream_endpoint(&model_id, &path_and_query)
        .await
    {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => match state.llama_swap.load_model(&model_id, false).await {
            Ok(outcome) if outcome.state == ControlState::Conflict => {
                return (StatusCode::CONFLICT, outcome.message).into_response()
            }
            Ok(_) => match state
                .llama_swap
                .ready_upstream_endpoint(&model_id, &path_and_query)
                .await
            {
                Ok(Some(endpoint)) => endpoint,
                Ok(None) => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "worker profile did not expose a ready endpoint",
                    )
                        .into_response()
                }
                Err(error) => return (StatusCode::BAD_GATEWAY, error).into_response(),
            },
            Err(error) => return (StatusCode::BAD_GATEWAY, error).into_response(),
        },
        Err(error) => return (StatusCode::BAD_GATEWAY, error).into_response(),
    };
    fleet_proxy::forward_streaming(&state.relay_client, method, headers, endpoint, body, None).await
}

#[derive(Default, Deserialize)]
struct StatusCapabilities {
    #[serde(default)]
    voice: bool,
}

fn compatible_profiles(profiles: &mut Vec<crate::domain::ModelProfile>, voice: bool) {
    // Pre-0.25 peers reject unknown capabilities. Hide only voice profiles
    // until requested, so a new workload cannot make an older peer go offline.
    if !voice {
        profiles.retain(|profile| profile.kind != WorkloadKind::Voice);
    }
}

async fn status(
    State(state): State<PeerServerState>,
    Query(capabilities): Query<StatusCapabilities>,
) -> Json<PeerStatusResponse> {
    let mut status = state.fleet.local_peer_status();
    compatible_profiles(&mut status.models, capabilities.voice);
    Json(status)
}

async fn harness_statuses(
    State(state): State<PeerServerState>,
) -> Json<Vec<harness_setup::HarnessSetupStatus>> {
    Json(harness_setup::statuses(&state.fleet.snapshot()))
}

async fn configure_harness(
    State(state): State<PeerServerState>,
    Json(request): Json<HarnessSetupRequest>,
) -> Response {
    match harness_setup::configure(
        request.harness,
        &state.fleet,
        &state.hermes,
        &state.opencode,
        &state.harnesses,
    ) {
        Ok(status) => Json(status).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response(),
    }
}

async fn load_model(
    State(state): State<PeerServerState>,
    Json(request): Json<LoadModelRequest>,
) -> Response {
    control_response(
        state
            .llama_swap
            .load_model_with_context(&request.model_id, request.force, request.context_window)
            .await,
    )
}

async fn unload_models(
    State(state): State<PeerServerState>,
    Json(request): Json<UnloadModelsRequest>,
) -> Response {
    control_response(state.llama_swap.unload_models(request.force).await)
}

async fn reload_model(
    State(state): State<PeerServerState>,
    Json(request): Json<LoadModelRequest>,
) -> Response {
    control_response(
        state
            .llama_swap
            .reload_model(&request.model_id, request.force, request.context_window)
            .await,
    )
}

async fn deliver_hermes(
    State(state): State<PeerServerState>,
    Json(request): Json<HarnessDeliveryRequest>,
) -> Response {
    if let Err(error) = request.validate() {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response();
    }
    let Some(native_session_id) = request.native_session_id.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "Hermes delivery requires native_session_id".into(),
            }),
        )
            .into_response();
    };
    let proxy_endpoint = state.fleet.snapshot().proxy_endpoint;
    match state
        .hermes
        .deliver_api_message(
            &request.host_id,
            &request.model_id,
            &proxy_endpoint,
            native_session_id,
            &request.idempotency_key,
            &request.text,
        )
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(ApiError { error })).into_response(),
    }
}

async fn deliver_opencode(
    State(state): State<PeerServerState>,
    Json(request): Json<HarnessDeliveryRequest>,
) -> Response {
    if let Err(error) = request.validate() {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response();
    }
    match state
        .opencode
        .deliver_api_message(&request, &state.fleet)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(ApiError { error })).into_response(),
    }
}

async fn opencode_sessions(State(state): State<PeerServerState>) -> Response {
    match state.opencode.list_sessions() {
        Ok(sessions) => Json(sessions).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response(),
    }
}

async fn deliver_pi(
    State(state): State<PeerServerState>,
    Json(request): Json<HarnessDeliveryRequest>,
) -> Response {
    if let Err(error) = request.validate() {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response();
    }
    match state.pi.deliver_message(&request, &state.fleet).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(ApiError { error })).into_response(),
    }
}

async fn set_harness_session_archived(
    State(state): State<PeerServerState>,
    Path(harness): Path<String>,
    Json(request): Json<HarnessSessionArchiveRequest>,
) -> Response {
    if let Err(error) = request.validate() {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response();
    }
    let result = match harness.as_str() {
        "hermes" => state
            .hermes
            .set_session_archived(&request.native_session_id, request.archived),
        "opencode" => state
            .opencode
            .set_session_archived(&request.native_session_id, request.archived),
        "pi" => state
            .pi
            .set_session_archived(&request.native_session_id, request.archived),
        _ => Err(format!("unsupported harness session archive: {harness}")),
    };
    match result {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response(),
    }
}

fn control_response(result: Result<ControlOutcome, String>) -> Response {
    match result {
        Ok(outcome) => {
            let status = if outcome.state == ControlState::Conflict {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            (status, Json(outcome)).into_response()
        }
        Err(error) => (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response(),
    }
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_peers_keep_text_inventory_without_new_voice_capabilities() {
        let voice: crate::domain::ModelProfile = serde_json::from_value(serde_json::json!({
            "id":"voice","display_name":"Voice","runtime":"mlx_voicechat","kind":"voice",
            "capabilities":["realtime_audio"]
        }))
        .unwrap();
        let text: crate::domain::ModelProfile = serde_json::from_value(serde_json::json!({
            "id":"text","display_name":"Text","runtime":"llama_cpp"
        }))
        .unwrap();
        assert!(!voice.supports_capability(&ProfileCapability::Chat));
        let mut profiles = vec![text, voice];
        compatible_profiles(&mut profiles, true);
        assert_eq!(profiles.len(), 2);
        compatible_profiles(&mut profiles, false);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "text");
    }

    #[test]
    fn parses_tailscale_address() {
        let address = parse_tailscale_ipv4(b"100.64.0.43\n");
        assert_eq!(address, Some(Ipv4Addr::new(100, 64, 0, 43)));
    }

    #[test]
    fn ignores_non_ipv4_tailscale_output() {
        assert_eq!(parse_tailscale_ipv4(b"fd7a:115c:a1e0::1\n"), None);
    }

    #[test]
    fn peer_api_retry_interval_is_short_enough_for_startup_races() {
        assert_eq!(RETRY_INTERVAL, Duration::from_secs(5));
    }
}

#[derive(Deserialize)]
struct AvailabilityRequest {
    available: bool,
}

#[derive(Deserialize)]
struct AutoPrepareRequest {
    model_id: String,
}

async fn prepare_auto(
    State(state): State<PeerServerState>,
    Json(request): Json<AutoPrepareRequest>,
) -> Response {
    match state
        .llama_swap
        .prepare_auto(&request.model_id, "v1/chat/completions")
        .await
    {
        Ok(()) => Json(serde_json::json!({"ready": true})).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

async fn reserve_auto(
    State(state): State<PeerServerState>,
    Json(request): Json<AutoPrepareRequest>,
) -> Response {
    match state.llama_swap.reserve_auto(&request.model_id).await {
        Ok(token) => Json(serde_json::json!({"reservation": token})).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

#[derive(Deserialize)]
struct ReleaseAutoRequest {
    reservation: String,
}

async fn release_auto(
    State(state): State<PeerServerState>,
    Json(request): Json<ReleaseAutoRequest>,
) -> Response {
    state
        .llama_swap
        .admissions
        .release_pending(&request.reservation);
    StatusCode::NO_CONTENT.into_response()
}

// Deliberately absent from agentrelayctl and loopback management actions.
// GUI peers use the existing trusted tailnet control channel.
async fn set_availability(
    State(state): State<PeerServerState>,
    Json(request): Json<AvailabilityRequest>,
) -> Response {
    match state.llama_swap.set_available(request.available).await {
        Ok(()) => Json(serde_json::json!({"available": request.available})).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}
