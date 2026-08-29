use std::{env, net::Ipv4Addr, path::PathBuf, process::Command, time::Duration};

use axum::{
    body::Body,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    channels::{HarnessDeliveryRequest, HarnessSessionArchiveRequest},
    domain::{
        ControlOutcome, ControlState, LoadModelRequest, PeerApiState, PeerApiStatus,
        PeerStatusResponse, ProfileCapability, UnloadModelsRequest, WorkloadKind,
    },
    fleet::SharedFleetService,
    fleet_proxy,
    gateway::SharedGatewayCoordinator,
    gateway_runtime::SharedGatewaySupervisor,
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
    gateway: SharedGatewayCoordinator,
    config_dir: PathBuf,
    gateway_supervisor: SharedGatewaySupervisor,
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
    gateway: SharedGatewayCoordinator,
    gateway_supervisor: SharedGatewaySupervisor,
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
        gateway,
        config_dir: PathBuf::from(fleet.snapshot().config_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf(),
        gateway_supervisor,
    };
    let app = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/control/load", post(load_model))
        .route("/api/v1/control/unload", post(unload_models))
        .route("/api/v1/harnesses", get(harness_statuses))
        .route("/api/v1/harness/configure", post(configure_harness))
        .route(
            "/api/v1/channels/gateway/config",
            post(configure_channel_gateway),
        )
        .route(
            "/api/v1/channels/gateway/provision",
            post(provision_channel_gateway),
        )
        .route("/api/v1/harness/hermes/deliver", post(deliver_hermes))
        .route("/api/v1/harness/opencode/sessions", get(opencode_sessions))
        .route("/api/v1/harness/opencode/deliver", post(deliver_opencode))
        .route("/api/v1/harness/pi/deliver", post(deliver_pi))
        .route(
            "/api/v1/harness/{harness}/session/archive",
            post(set_harness_session_archived),
        )
        .route("/api/v1/comfy/{model_id}/{*path}", any(proxy_comfy_request))
        .route("/api/v1/proxy/{*path}", any(proxy_request))
        .route("/metrics", get(prometheus_metrics))
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
    gateway: SharedGatewayCoordinator,
    gateway_supervisor: SharedGatewaySupervisor,
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
                    gateway.clone(),
                    gateway_supervisor.clone(),
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
    headers: HeaderMap,
    body: Body,
) -> Response {
    let observes_generation = fleet_proxy::is_generation_path(&path);
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let endpoint = state.fleet.local_llama_swap_endpoint(&path_and_query);
    let observer = if observes_generation {
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
    fleet_proxy::forward_streaming(
        &state.relay_client,
        method,
        headers,
        endpoint,
        body,
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
        .ready_model_endpoint(&model_id, &path_and_query)
        .await
    {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => match state.llama_swap.load_model(&model_id, false).await {
            Ok(outcome) if outcome.state == ControlState::Conflict => {
                return (StatusCode::CONFLICT, outcome.message).into_response()
            }
            Ok(_) => match state
                .llama_swap
                .ready_model_endpoint(&model_id, &path_and_query)
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

async fn status(State(state): State<PeerServerState>) -> Json<PeerStatusResponse> {
    Json(state.fleet.local_peer_status())
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

async fn configure_channel_gateway(
    State(state): State<PeerServerState>,
    Json(config): Json<crate::config::ChannelGatewayConfig>,
) -> Response {
    match crate::config::set_channel_gateway_config(&state.config_dir, config.clone())
        .and_then(|saved| state.gateway.update_config(saved).map(|_| config))
    {
        Ok(config) => {
            state.gateway_supervisor.restart();
            Json(config).into_response()
        }
        Err(error) => (StatusCode::BAD_REQUEST, Json(ApiError { error })).into_response(),
    }
}

#[derive(Deserialize)]
struct GatewayProvisionRequest {
    config: crate::config::ChannelGatewayConfig,
    project_secret: String,
}

async fn provision_channel_gateway(
    State(state): State<PeerServerState>,
    Json(request): Json<GatewayProvisionRequest>,
) -> Response {
    let result =
        crate::config::set_channel_gateway_config(&state.config_dir, request.config.clone())
            .and_then(|saved| state.gateway.update_config(saved))
            .and_then(|_| {
                state
                    .gateway_supervisor
                    .store_secret(&request.project_secret)
            });
    match result {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
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
