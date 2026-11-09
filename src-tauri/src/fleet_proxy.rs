use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    config,
    domain::{
        ConnectionState, ControlOutcome, ControlState, FleetLoadModelRequest,
        FleetUnloadModelsRequest, InferenceOverrides, LoadModelRequest, ModelProfile,
        ProfileCapability, ReasoningEffort, UnloadModelsRequest, WorkloadKind,
    },
    fleet::SharedFleetService,
    hermes_bridge::{HermesPresence, HermesSwitchAck, SharedHermesBridge},
    llama_swap::SharedLlamaSwapSupervisor,
    telemetry::{now_ms, RequestTelemetry, SharedTelemetry},
};

const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
/// A queued request for a client at its `max_concurrent` limit waits up to this
/// long: long enough for a full agent reply, released if the client disconnects.
const CLIENT_QUEUE_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_COMFY_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;
const GENERATION_DRAINING: u64 = 1 << 63;
const GENERATION_COUNT_MASK: u64 = !GENERATION_DRAINING;
pub(crate) const ROUTED_MODEL_ID: &str = "agentrelay";
static GENERATION_GATE: GenerationGate = GenerationGate::new();

pub(crate) fn client_proxy_base_url(proxy_endpoint: &str, client: &str) -> String {
    format!(
        "{}/clients/{client}/v1",
        proxy_endpoint.trim_end_matches('/')
    )
}

pub(crate) fn begin_generation_drain() -> Result<GenerationDrain<'static>, u32> {
    GENERATION_GATE.begin_drain()
}

pub(crate) fn track_voice_session() -> Option<ActiveGenerationRequest<'static>> {
    GENERATION_GATE.try_track()
}

#[derive(Clone)]
struct ProxyState {
    app: tauri::AppHandle,
    fleet: SharedFleetService,
    llama_swap: SharedLlamaSwapSupervisor,
    hermes_bridge: SharedHermesBridge,
    client: reqwest::Client,

    telemetry: SharedTelemetry,
}

#[derive(Clone)]
pub(crate) struct ProxyIntegrations {
    pub(crate) app: tauri::AppHandle,
    pub(crate) hermes_bridge: SharedHermesBridge,
}

#[derive(Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ProxyModel>,
}

#[derive(Serialize)]
struct ProxyModel {
    id: String,
    object: &'static str,
    created: u8,
    owned_by: String,
    display_name: String,
    runtime: String,
    online: bool,
}

pub async fn serve(
    fleet: SharedFleetService,
    integrations: ProxyIntegrations,
    llama_swap: SharedLlamaSwapSupervisor,

    telemetry: SharedTelemetry,
) -> Result<(), String> {
    let listen_address = fleet.proxy_listen_address().to_owned();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(1_500))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to create fleet proxy HTTP client: {error}"))?;
    let hermes_bridge_routes = Router::new()
        .route("/agent-relay/hermes/presence", post(hermes_presence))
        .route("/agent-relay/hermes/ack", post(hermes_ack))
        .route("/agent-relay/hermes/status", get(hermes_status))
        .layer(hermes_bridge_cors());
    let app = Router::new()
        .route("/api/v1/status", get(fleet_status))
        .route("/api/v1/routing/waiting", get(routing_waiting))
        .route(
            "/api/v1/models/{model_id}/capacity",
            get(local_model_capacity),
        )
        .route("/api/v1/manage", post(manage_cli))
        .route("/api/v1/control/load", post(fleet_load_model))
        .route("/api/v1/control/unload", post(fleet_unload_models))
        .route("/metrics", get(prometheus_metrics))
        .route(
            "/api/comfy/{host_id}/{model_id}/{*path}",
            any(comfy_proxy_request),
        )
        .route(
            "/api/worker/{host_id}/{model_id}/{*path}",
            any(worker_proxy_request),
        )
        .route("/clients/{client}/v1/models", get(client_models))
        .route("/clients/{client}/v1/{*path}", any(client_proxy_request))
        .route("/v1/models", get(models))
        .route("/v1/{*path}", any(proxy_request))
        .merge(hermes_bridge_routes)
        .merge(crate::voice::local_router(
            fleet.clone(),
            llama_swap.clone(),
        ))
        .with_state(ProxyState {
            app: integrations.app,
            fleet,
            llama_swap,
            hermes_bridge: integrations.hermes_bridge,
            client,

            telemetry,
        });
    let listener = tokio::net::TcpListener::bind(&listen_address)
        .await
        .map_err(|error| format!("failed to bind fleet proxy to {listen_address}: {error}"))?;

    axum::serve(listener, app)
        .await
        .map_err(|error| format!("fleet proxy stopped: {error}"))
}

async fn local_model_capacity(
    State(state): State<ProxyState>,
    Path(model_id): Path<String>,
) -> Response {
    let snapshot = state.fleet.snapshot();
    let profile = snapshot
        .hosts
        .iter()
        .find(|h| h.id == snapshot.local_host_id)
        .and_then(|h| h.models.iter().find(|p| p.id == model_id));
    let Some(profile) = profile else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_model",
            "unknown local model",
        );
    };
    if profile.runtime != "llama.cpp" {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "unsupported_runtime",
            "slot inspection requires llama.cpp",
        );
    }
    match state.llama_swap.model_capacity(&model_id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity_unavailable",
            error,
        ),
    }
}

async fn manage_cli(
    State(state): State<ProxyState>,
    Json(body): Json<crate::cli_control::Request>,
) -> Response {
    match crate::cli_control::execute(state.app, body).await {
        Ok(value) => {
            let status = if value.get("state").and_then(Value::as_str) == Some("conflict") {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            (status, Json(value)).into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

async fn routing_waiting(
    State(state): State<ProxyState>,
) -> Json<std::collections::HashMap<String, u32>> {
    Json(state.fleet.routing_waiting.snapshot())
}

async fn fleet_status(State(state): State<ProxyState>) -> Json<crate::domain::FleetSnapshot> {
    Json(state.fleet.refresh().await)
}

async fn prometheus_metrics(State(state): State<ProxyState>) -> Response {
    match state.telemetry.prometheus(&state.fleet.snapshot()) {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn comfy_proxy_request(
    State(state): State<ProxyState>,
    Path((host_id, model_id, path)): Path<(String, String, String)>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !comfy_path_allowed(&path, &method) {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unsupported_comfy_route",
            format!("ComfyUI route is not exposed: {method} /{path}"),
        );
    }
    if headers
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return openai_error(
            StatusCode::NOT_IMPLEMENTED,
            "websocket_not_supported",
            "ComfyUI WebSocket relay is not available in this release",
        );
    }

    let snapshot = state.fleet.snapshot();
    let Some(host) = snapshot.hosts.iter().find(|host| host.id == host_id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_host",
            format!("unknown fleet host: {host_id}"),
        );
    };
    if !host.available {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_unavailable",
            "This machine is unavailable for new work",
        );
    }
    if !host.available {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_unavailable",
            "This machine is unavailable for new work",
        );
    }
    if host.connection == ConnectionState::Offline {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_offline",
            format!("{} is offline", host.display_name),
        );
    }
    let Some(profile) = host.models.iter().find(|profile| profile.id == model_id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_profile",
            format!("{} has no profile named {model_id}", host.display_name),
        );
    };
    if profile.kind != WorkloadKind::Image
        || !profile
            .capabilities
            .contains(&ProfileCapability::WorkflowQueue)
    {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "unsupported_workload",
            format!("{model_id} is not a ComfyUI workflow profile"),
        );
    }

    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let endpoint = if state.fleet.is_local_host(&host_id) {
        match local_upstream_endpoint(&state, &model_id, &path_and_query).await {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return openai_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "profile_unavailable",
                    error,
                )
            }
        }
    } else {
        match state
            .fleet
            .peer_comfy_endpoint(&host_id, &model_id, &path_and_query)
        {
            Ok(endpoint) => endpoint,
            Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_host", error),
        }
    };
    let body = match to_bytes(body, MAX_COMFY_REQUEST_BODY_BYTES).await {
        Ok(body) => body.to_vec(),
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request", error),
    };
    forward_buffered(&state.client, method, headers, endpoint, body, None).await
}

/// Proxy a request to a supervised HTTP worker profile (`kind: worker`) on any host.
/// Mirrors the ComfyUI passthrough but streams the body, because worker responses
/// (embeddings, masks) can be large.
async fn worker_proxy_request(
    State(state): State<ProxyState>,
    Path((host_id, model_id, path)): Path<(String, String, String)>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !worker_path_allowed(&path, &method) {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unsupported_worker_route",
            format!("worker route is not exposed: {method} /{path}"),
        );
    }
    if headers
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return openai_error(
            StatusCode::NOT_IMPLEMENTED,
            "websocket_not_supported",
            "worker WebSocket relay is not available",
        );
    }

    let snapshot = state.fleet.snapshot();
    let Some(host) = snapshot.hosts.iter().find(|host| host.id == host_id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_host",
            format!("unknown fleet host: {host_id}"),
        );
    };
    if !host.available {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_unavailable",
            "This machine is unavailable for new work",
        );
    }
    if host.connection == ConnectionState::Offline {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_offline",
            format!("{} is offline", host.display_name),
        );
    }
    let Some(profile) = host.models.iter().find(|profile| profile.id == model_id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_profile",
            format!("{} has no profile named {model_id}", host.display_name),
        );
    };
    if !profile.is_worker_service() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "unsupported_workload",
            format!("{model_id} is not a worker service profile"),
        );
    }

    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let endpoint = if state.fleet.is_local_host(&host_id) {
        match local_upstream_endpoint(&state, &model_id, &path_and_query).await {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return openai_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "profile_unavailable",
                    error,
                )
            }
        }
    } else {
        match state
            .fleet
            .peer_worker_endpoint(&host_id, &model_id, &path_and_query)
        {
            Ok(endpoint) => endpoint,
            Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_host", error),
        }
    };
    forward_streaming(&state.client, method, headers, endpoint, body, None).await
}

/// Worker profiles expose a health probe and a versioned API surface only.
pub(crate) fn worker_path_allowed(path: &str, method: &Method) -> bool {
    let mut segments = path.split('/');
    let root = segments.next().unwrap_or_default();
    match root {
        "health" => *method == Method::GET && segments.next().is_none(),
        "v1" => {
            segments.next().is_some_and(|segment| !segment.is_empty())
                && (*method == Method::GET || *method == Method::POST)
        }
        _ => false,
    }
}

pub(crate) fn comfy_path_allowed(path: &str, method: &Method) -> bool {
    let root = path.split('/').next().unwrap_or_default();
    match root {
        "prompt" | "history" | "queue" => *method == Method::GET || *method == Method::POST,
        "view" | "system_stats" => *method == Method::GET,
        "interrupt" | "free" => *method == Method::POST,
        _ => false,
    }
}

async fn fleet_load_model(
    State(state): State<ProxyState>,
    Json(request): Json<FleetLoadModelRequest>,
) -> Response {
    let snapshot = state.fleet.refresh().await;
    let Some(host) = snapshot
        .hosts
        .iter()
        .find(|host| host.id == request.host_id)
    else {
        return management_error(
            StatusCode::NOT_FOUND,
            format!("unknown fleet host: {}", request.host_id),
        );
    };
    if !host.available {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_unavailable",
            "This machine is unavailable for new work",
        );
    }
    if host.connection == ConnectionState::Offline {
        return management_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{} is offline", host.display_name),
        );
    }
    if !host.models.iter().any(|model| model.id == request.model_id) {
        return management_error(
            StatusCode::NOT_FOUND,
            format!(
                "{} has no profile named {}",
                host.display_name, request.model_id
            ),
        );
    }

    let result = if state.fleet.is_local_host(&request.host_id) {
        state
            .llama_swap
            .load_model_with_context(&request.model_id, request.force, request.context_window)
            .await
    } else {
        state
            .fleet
            .request_peer_load(
                &request.host_id,
                &LoadModelRequest {
                    model_id: request.model_id,
                    force: request.force,
                    context_window: request.context_window,
                },
            )
            .await
    };
    management_control_response(&state, result).await
}

async fn fleet_unload_models(
    State(state): State<ProxyState>,
    Json(request): Json<FleetUnloadModelsRequest>,
) -> Response {
    let snapshot = state.fleet.refresh().await;
    let Some(host) = snapshot
        .hosts
        .iter()
        .find(|host| host.id == request.host_id)
    else {
        return management_error(
            StatusCode::NOT_FOUND,
            format!("unknown fleet host: {}", request.host_id),
        );
    };
    if !host.available {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_unavailable",
            "This machine is unavailable for new work",
        );
    }
    if host.connection == ConnectionState::Offline {
        return management_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{} is offline", host.display_name),
        );
    }

    let result = if state.fleet.is_local_host(&request.host_id) {
        state.llama_swap.unload_models(request.force).await
    } else {
        state
            .fleet
            .request_peer_unload(
                &request.host_id,
                &UnloadModelsRequest {
                    force: request.force,
                },
            )
            .await
    };
    management_control_response(&state, result).await
}

async fn management_control_response(
    state: &ProxyState,
    result: Result<ControlOutcome, String>,
) -> Response {
    match result {
        Ok(outcome) => {
            state.fleet.refresh().await;
            let status = if outcome.state == ControlState::Conflict {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            (status, Json(outcome)).into_response()
        }
        Err(error) => management_error(StatusCode::BAD_GATEWAY, error),
    }
}

fn management_error(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        Json(ManagementError {
            error: error.into(),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct ManagementError {
    error: String,
}

fn hermes_bridge_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            trusted_hermes_origin(origin)
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
}

fn trusted_hermes_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if matches!(
        origin,
        // Packaged Electron pages have an opaque origin and serialize it as
        // `null`. The Tauri entries support local development and packaging.
        "null" | "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) {
        return true;
    }

    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    authority.is_some_and(trusted_loopback_authority)
}

fn trusted_loopback_authority(authority: &str) -> bool {
    let (host, port) = if let Some(port) = authority.strip_prefix("[::1]:") {
        ("[::1]", Some(port))
    } else if authority == "[::1]" {
        ("[::1]", None)
    } else if let Some((host, port)) = authority.split_once(':') {
        (host, Some(port))
    } else {
        (authority, None)
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        && port.is_none_or(|port| !port.is_empty() && port.parse::<u16>().is_ok())
}

async fn hermes_presence(
    State(state): State<ProxyState>,
    Json(presence): Json<HermesPresence>,
) -> Json<crate::hermes_bridge::HermesPresenceResponse> {
    Json(state.hermes_bridge.presence(presence))
}

async fn hermes_ack(
    State(state): State<ProxyState>,
    Json(ack): Json<HermesSwitchAck>,
) -> StatusCode {
    if state.hermes_bridge.acknowledge(ack) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CONFLICT
    }
}

async fn hermes_status(
    State(state): State<ProxyState>,
) -> Json<crate::hermes_bridge::HermesBridgeStatus> {
    Json(state.hermes_bridge.status())
}

async fn models(State(state): State<ProxyState>) -> Json<ModelsResponse> {
    let snapshot = state.fleet.snapshot();
    let data = snapshot
        .hosts
        .into_iter()
        .flat_map(|host| {
            let online = host.connection != ConnectionState::Offline;
            host.models
                .into_iter()
                .filter(|model| {
                    model.supports_text_inference()
                        || model.supports_capability(&crate::domain::ProfileCapability::Embeddings)
                })
                .map(move |model| ProxyModel {
                    id: format!("{}/{}", host.id, model.id),
                    object: "model",
                    created: 0,
                    owned_by: host.id.clone(),
                    display_name: model.display_name,
                    runtime: model.runtime,
                    online,
                })
        })
        .collect();

    Json(ModelsResponse {
        object: "list",
        data,
    })
}

async fn client_models(State(state): State<ProxyState>, Path(client): Path<String>) -> Response {
    let snapshot = state.fleet.snapshot();
    let selected_model = match client_selected_model(&snapshot, &client) {
        Ok(selected_model) => selected_model,
        Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_client", error),
    };
    let mut online = selected_model
        .and_then(|qualified| qualified.split_once('/'))
        .and_then(|(host_id, model_id)| {
            snapshot
                .hosts
                .iter()
                .find(|host| host.id == host_id)
                .map(|host| {
                    host.available
                        && host.connection != ConnectionState::Offline
                        && host.loaded_model_id.as_deref() == Some(model_id)
                })
        })
        .unwrap_or(false);

    if let Ok(routing) = state.fleet.routing_config() {
        let policy = routing.policy(&client);
        if policy.mode == crate::routing::RouteMode::Auto {
            let needs = crate::routing::Requirements {
                capability: if client == "codex" {
                    ProfileCapability::Responses
                } else if client == "claude_code" {
                    ProfileCapability::AnthropicMessages
                } else {
                    ProfileCapability::Chat
                },
                vision: false,
                tools: false,
                context: policy.minimum_context,
            };
            online =
                !crate::routing::candidates(&routing, &client, &snapshot, &needs, None).is_empty();
        }
    }
    Json(ModelsResponse {
        object: "list",
        data: vec![ProxyModel {
            id: ROUTED_MODEL_ID.to_owned(),
            object: "model",
            created: 0,
            owned_by: "agentrelay".into(),
            display_name: "Agent Relay".into(),
            runtime: "routed".into(),
            online,
        }],
    })
    .into_response()
}

async fn client_proxy_request(
    State(state): State<ProxyState>,
    Path((client, path)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let snapshot = state.fleet.snapshot();
    let bytes = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request", error),
    };
    let routing = match state.fleet.routing_config() {
        Ok(routing) => routing,
        Err(error) => {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "routing_config_error",
                error,
            )
        }
    };
    let policy = routing.policy(&client);
    // Queue before routing: while this client's own request runs, the next one
    // waits for it instead of seeing a full preferred model and spilling over.
    let client_permit = match acquire_client_slot(
        &state.fleet,
        &policy,
        &client,
        &path,
        CLIENT_QUEUE_TIMEOUT,
    )
    .await
    {
        Ok(permit) => permit,
        Err(error) => return client_busy(error),
    };
    let auto = policy.mode == crate::routing::RouteMode::Auto;
    let affinity = session_affinity(&headers);
    let mut reservation = None;
    let mut headers = headers;
    headers.remove("x-agentrelay-reservation");
    headers.remove("x-agentrelay-auto");
    let selected_model = if auto {
        let previous = state.fleet.session_route(&client, affinity);
        let started = Instant::now();
        let mut waiting = None;
        let selected = loop {
            let current = state.fleet.snapshot();
            let current_routing = match state.fleet.routing_config() {
                Ok(c) if c.policy(&client).mode == crate::routing::RouteMode::Auto => c,
                _ => {
                    return openai_error(
                        StatusCode::CONFLICT,
                        "routing_changed",
                        "Routing changed while waiting; prompt not submitted",
                    )
                }
            };
            let requirements = match crate::routing::Requirements::parse(
                &path,
                &bytes,
                current_routing.policy(&client).minimum_context,
            ) {
                Ok(value) => value,
                Err(error) => {
                    return openai_error(StatusCode::BAD_REQUEST, "unsupported_auto_request", error)
                }
            };
            let choices = crate::routing::candidates(
                &current_routing,
                &client,
                &current,
                &requirements,
                previous.as_deref(),
            );
            let reserved = crate::routing::reserve_first(choices, |candidate| {
                let state = &state;
                async move {
                    let (host, model) = candidate.split_once('/').expect("validated candidate");
                    let local = state.fleet.is_local_host(host);
                    let token = if local {
                        state.llama_swap.reserve_auto(model).await
                    } else {
                        state.fleet.reserve_peer_auto(host, model).await
                    }?;
                    Ok(AutoReservation {
                        token,
                        host: host.into(),
                        fleet: state.fleet.clone(),
                        local: local.then(|| state.llama_swap.admissions.clone()),
                    })
                }
            })
            .await;
            let failures = match reserved {
                Ok((model, ticket)) => {
                    reservation = Some(ticket);
                    break model;
                }
                Err(failures) => failures,
            };
            let busy = failures
                .iter()
                .any(|error| error.contains(": capacity_busy:"));
            if busy && started.elapsed() < Duration::from_secs(30) {
                if waiting.is_none() {
                    let guard = match state.fleet.routing_waiting.enter(&client) {
                        Ok(g) => g,
                        Err(e) => {
                            return openai_error(
                                StatusCode::TOO_MANY_REQUESTS,
                                "routing_queue_full",
                                e,
                            )
                        }
                    };
                    let Some(active) = GENERATION_GATE.try_track() else {
                        return openai_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "proxy_draining",
                            "Relay is shutting down",
                        );
                    };
                    waiting = Some((guard, active));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            let reason = if failures.is_empty() {
                crate::routing::rejection_message(
                    &current_routing,
                    &client,
                    &current,
                    &requirements,
                )
            } else {
                format!(
                    "Approved models could not admit this request: {}",
                    failures.join("; ")
                )
            };
            let mut response = openai_error(
                if busy {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                },
                if busy {
                    "models_busy"
                } else {
                    "no_eligible_model"
                },
                reason,
            );
            if busy {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            }
            return response;
        };
        headers.insert("x-agentrelay-auto", HeaderValue::from_static("true"));
        headers.insert(
            "x-agentrelay-reservation",
            HeaderValue::from_str(&reservation.as_ref().expect("reserved").token)
                .expect("UUID header"),
        );
        selected
    } else {
        match client_selected_model(&snapshot, &client) {
            Ok(Some(selected_model)) => selected_model.to_owned(),
            Ok(None) => {
                return openai_error(
                    StatusCode::CONFLICT,
                    "route_not_selected",
                    format!("choose an Agent Relay target for {client} first"),
                )
            }
            Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_client", error),
        }
    };
    let (body, host_id, model_id) = match rewrite_model_to_route(&bytes, &selected_model) {
        Ok(value) => value,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_model", error),
    };
    let body = if client == "codex" && path == "responses" {
        match normalize_codex_instructions(&body) {
            Ok(body) => body,
            Err(error) => {
                return openai_error(StatusCode::BAD_REQUEST, "invalid_codex_input", error)
            }
        }
    } else {
        body
    };
    proxy_target_request(
        &state,
        TargetRequest {
            path,
            uri,
            method,
            headers,
            body,
            client,
            reservation,
            client_permit,
        },
        host_id,
        model_id,
    )
    .await
}

/// Wait for this client's `max_concurrent` slot. Only generation requests count;
/// model listings and other reads never queue. The tray shows queued requests.
async fn acquire_client_slot(
    fleet: &SharedFleetService,
    policy: &crate::routing::ClientPolicy,
    client: &str,
    path: &str,
    timeout: Duration,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, String> {
    let Some(limit) = policy.max_concurrent.filter(|_| is_generation_path(path)) else {
        return Ok(None);
    };
    let queued = fleet.routing_waiting.enter(client).ok();
    let permit = fleet.client_limits.acquire(client, limit, timeout).await;
    drop(queued);
    permit.map(Some)
}

/// Retryable refusal for a request that waited too long for its client slot.
fn client_busy(error: String) -> Response {
    let mut response = openai_error(StatusCode::TOO_MANY_REQUESTS, "client_busy", error);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

// Qwen-family templates accept system instructions only at the beginning.
// Keep trusted system/developer text in Responses instructions; never promote
// user or tool output into this field, and retain the order of conversation items.
fn normalize_codex_instructions(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut payload: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    let Some(items) = payload.get("input").and_then(Value::as_array) else {
        return Ok(body.to_vec());
    };
    let mut instructions = Vec::new();
    if let Some(existing) = payload.get("instructions").filter(|value| !value.is_null()) {
        instructions.push(
            existing
                .as_str()
                .ok_or("instructions must be text")?
                .to_owned(),
        );
    }
    let mut conversation = Vec::new();
    let mut changed = false;
    for item in items {
        if matches!(
            item.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        ) {
            let content = item
                .get("content")
                .ok_or("instruction message has no content")?;
            if let Some(text) = content.as_str() {
                instructions.push(text.to_owned());
            } else if let Some(parts) = content.as_array() {
                for part in parts {
                    instructions.push(
                        part.get("text")
                            .and_then(Value::as_str)
                            .ok_or("instruction content must be text")?
                            .to_owned(),
                    );
                }
            } else {
                return Err("instruction content must be text".into());
            }
            changed = true;
        } else {
            conversation.push(item.clone());
        }
    }
    if !changed {
        return Ok(body.to_vec());
    }
    payload["instructions"] = Value::String(instructions.join("\n\n"));
    payload["input"] = Value::Array(conversation);
    serde_json::to_vec(&payload).map_err(|error| error.to_string())
}

async fn proxy_request(
    State(state): State<ProxyState>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let mut headers = headers;
    headers.remove("x-agentrelay-auto");
    headers.remove("x-agentrelay-reservation");
    let bytes = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request", error),
    };
    let (body, host_id, model_id) = match rewrite_model(&bytes) {
        Ok(value) => value,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_model", error),
    };
    proxy_target_request(
        &state,
        TargetRequest {
            path,
            uri,
            method,
            headers,
            body,
            client: "openai".into(),
            reservation: None,
            client_permit: None,
        },
        host_id,
        model_id,
    )
    .await
}

struct AutoReservation {
    token: String,
    host: String,
    fleet: SharedFleetService,
    local: Option<std::sync::Arc<crate::admission::Admissions>>,
}
impl Drop for AutoReservation {
    fn drop(&mut self) {
        if let Some(pool) = &self.local {
            pool.release_pending(&self.token);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (fleet, host, token) = (self.fleet.clone(), self.host.clone(), self.token.clone());
            runtime.spawn(async move {
                fleet.release_peer_auto(&host, &token).await;
            });
        }
    }
}

// Keep only an opaque in-memory affinity key, never a session identifier in telemetry.
fn session_affinity(headers: &HeaderMap) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let value = headers.get("x-agentrelay-session")?.to_str().ok()?;
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hash);
    Some(hash.finish())
}

struct TargetRequest {
    path: String,
    uri: axum::http::Uri,
    method: Method,
    headers: HeaderMap,
    body: Vec<u8>,
    client: String,
    reservation: Option<AutoReservation>,
    client_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

async fn proxy_target_request(
    state: &ProxyState,
    mut request: TargetRequest,
    host_id: String,
    model_id: String,
) -> Response {
    let snapshot = state.fleet.snapshot();
    let Some(host) = snapshot.hosts.iter().find(|host| host.id == host_id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_host",
            format!("unknown fleet host: {host_id}"),
        );
    };
    if !host.available {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_unavailable",
            "This machine is unavailable for new work",
        );
    }
    if host.connection == ConnectionState::Offline {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_offline",
            format!("{} is offline", host.display_name),
        );
    }
    let Some(profile) = host.models.iter().find(|model| model.id == model_id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "unknown_model",
            format!("{} has no profile named {model_id}", host.display_name),
        );
    };
    if profile.kind != WorkloadKind::Text {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "unsupported_workload",
            format!("{model_id} is not a text inference profile"),
        );
    }

    let body = if is_generation_path(&request.path) {
        let qualified_model = format!("{host_id}/{model_id}");
        let config_path = std::path::Path::new(&snapshot.config_path);
        let inference_override = config_path
            .parent()
            .and_then(|directory| config::get_inference_overrides(directory).ok())
            .and_then(|overrides| overrides.get(&qualified_model).cloned())
            .unwrap_or_default();
        match apply_inference_controls(&request.path, &request.body, profile, &inference_override) {
            Ok(body) => body,
            Err(error) => {
                return openai_error(StatusCode::BAD_REQUEST, "invalid_inference_controls", error)
            }
        }
    } else {
        request.body
    };

    let path_and_query = match request.uri.query() {
        Some(query) => format!("v1/{}?{query}", request.path),
        None => format!("v1/{}", request.path),
    };
    let slot = if state.fleet.is_local_host(&host_id)
        && is_generation_path(&request.path)
        && !profile.is_independent_embedding_service()
    {
        if let Some(reservation) = &request.reservation {
            match state
                .llama_swap
                .admissions
                .claim(&reservation.token, &model_id)
            {
                Ok(lease) => Some(lease),
                Err(e) => return openai_error(StatusCode::CONFLICT, "reservation_expired", e),
            }
        } else {
            match state.llama_swap.track_inference(&model_id) {
                Ok(lease) => Some(lease),
                Err(error) => return background_busy(error),
            }
        }
    } else {
        None
    };
    let mut admission = None;
    let endpoint = if state.fleet.is_local_host(&host_id) {
        match state
            .llama_swap
            .prepare_inference(
                &model_id,
                &path_and_query,
                request.headers.contains_key("x-agentrelay-auto"),
            )
            .await
        {
            Ok((endpoint, guard)) => {
                admission = Some(guard);
                endpoint
            }
            Err(error) => {
                return if error.starts_with("capacity_busy:")
                    || error.starts_with("host_occupied:")
                    || error.starts_with("host_reclaimed:")
                {
                    background_busy(error)
                } else {
                    openai_error(StatusCode::SERVICE_UNAVAILABLE, "model_unavailable", error)
                }
            }
        }
    } else {
        match state.fleet.peer_proxy_endpoint(&host_id, &path_and_query) {
            Ok(endpoint) => endpoint,
            Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_host", error),
        }
    };

    let mut observer = if is_generation_path(&request.path) {
        match GenerationObserver::try_new_tracked(
            state.fleet.clone(),
            state.telemetry.clone(),
            host_id.clone(),
            model_id.clone(),
            request.client.clone(),
        ) {
            Ok(observer) => Some(observer),
            Err(error) => {
                return openai_error(StatusCode::SERVICE_UNAVAILABLE, "proxy_draining", error)
            }
        }
    } else {
        None
    };
    if let Some(observer) = &mut observer {
        observer.hold_slot(slot);
    }
    drop(admission);
    if request.headers.contains_key("x-agentrelay-auto") {
        state
            .fleet
            .note_route_choice(&request.client, &format!("{host_id}/{model_id}"));
    }
    let auto = request.headers.contains_key("x-agentrelay-auto");
    if auto {
        state.fleet.note_session_route(
            &request.client,
            session_affinity(&request.headers),
            &format!("{host_id}/{model_id}"),
        );
    }
    if let Some(observer) = &mut observer {
        observer.reservation = request.reservation;
        // Held until the response body finishes or the client disconnects.
        observer.client_permit = request.client_permit;
    }
    if state.fleet.is_local_host(&host_id) {
        request.headers.remove("x-agentrelay-reservation");
        request.headers.remove("x-agentrelay-auto");
    }
    let mut response = forward_buffered(
        &state.client,
        request.method,
        request.headers,
        endpoint,
        body,
        observer,
    )
    .await;
    if let Ok(value) = HeaderValue::from_str(&format!("{host_id}/{model_id}")) {
        response.headers_mut().insert("x-agentrelay-model", value);
    }
    response.headers_mut().insert(
        "x-agentrelay-route-mode",
        HeaderValue::from_static(if auto { "auto" } else { "pinned" }),
    );
    response
}

fn apply_inference_controls(
    request_path: &str,
    body: &[u8],
    profile: &ModelProfile,
    inference_override: &InferenceOverrides,
) -> Result<Vec<u8>, String> {
    let controls = &profile.inference_controls;
    if controls.thinking.is_none() && controls.temperature.is_none() {
        return Ok(body.to_vec());
    }
    profile.validate_inference_override(inference_override)?;
    let mut payload: Value = serde_json::from_slice(body)
        .map_err(|error| format!("request body must be JSON: {error}"))?;

    if let Some(thinking) = controls.thinking.as_ref() {
        let effort = inference_override
            .reasoning_effort
            .or(thinking.default_effort);
        let effort_is_override = inference_override.reasoning_effort.is_some();
        let budget = inference_override
            .reasoning_budget
            .or(thinking.default_budget);
        let budget_is_override = inference_override.reasoning_budget.is_some();
        match thinking.adapter.as_str() {
            "strata" => {
                if let Some(effort) = effort {
                    let value = match effort {
                        ReasoningEffort::Off => "none",
                        ReasoningEffort::Low => "low",
                        ReasoningEffort::Medium => "medium",
                        ReasoningEffort::High => "high",
                        _ => return Err("Strata supports off, low, medium, and high".into()),
                    };
                    if effort_is_override {
                        clear_reasoning_effort(&mut payload, request_path)?;
                        if let Some(kwargs) = payload
                            .get_mut("chat_template_kwargs")
                            .and_then(Value::as_object_mut)
                        {
                            kwargs.remove("enable_thinking");
                        }
                    }
                    if effort_is_override
                        || (payload.get("reasoning_effort").is_none()
                            && payload.pointer("/reasoning/effort").is_none())
                    {
                        payload["reasoning_effort"] = Value::String(value.into());
                    }
                }
                if let Some(budget) = budget {
                    if budget < 0 {
                        return Err(
                            "Strata thinking budget must be nonnegative (0 means unlimited)".into(),
                        );
                    }
                    if budget_is_override || payload.get("reasoning_budget_tokens").is_none() {
                        payload["reasoning_budget_tokens"] = Value::Number(budget.into());
                    }
                }
            }
            "llama_cpp" => {
                if let Some(effort) = effort {
                    let effort_is_missing = if request_path == "responses" {
                        payload.pointer("/reasoning/effort").is_none()
                    } else {
                        payload.get("reasoning_effort").is_none()
                    };
                    if effort_is_override || effort_is_missing {
                        if effort == ReasoningEffort::Off {
                            // `none` is a model-specific reasoning effort (for
                            // example, GPT-OSS), not llama.cpp's universal
                            // spelling for disabled thinking. Qwen templates
                            // reject it. Disable thinking through the template
                            // flag and budget, and remove any overridden effort.
                            clear_reasoning_effort(&mut payload, request_path)?;
                            set_chat_template_thinking(&mut payload, false, effort_is_override)?;
                            if effort_is_override || payload.get("thinking_budget_tokens").is_none()
                            {
                                payload["thinking_budget_tokens"] = Value::Number(0.into());
                            }
                        } else {
                            set_reasoning_effort(
                                &mut payload,
                                request_path,
                                effort,
                                effort_is_override,
                            )?;
                        }
                    }
                }
                if effort != Some(ReasoningEffort::Off) {
                    if let Some(budget) = budget {
                        if budget_is_override || payload.get("thinking_budget_tokens").is_none() {
                            payload["thinking_budget_tokens"] = Value::Number(budget.into());
                        }
                    }
                }
            }
            "llama_cpp_toggle" => {
                if let Some(effort) = effort {
                    let enabled = match effort {
                        ReasoningEffort::Off => false,
                        ReasoningEffort::On => true,
                        _ => {
                            return Err("llama_cpp_toggle supports only off and on".to_owned());
                        }
                    };
                    set_chat_template_thinking(&mut payload, enabled, effort_is_override)?;
                    if !enabled
                        && (effort_is_override || payload.get("thinking_budget_tokens").is_none())
                    {
                        payload["thinking_budget_tokens"] = Value::Number(0.into());
                    }
                }
                if effort != Some(ReasoningEffort::Off) {
                    if let Some(budget) = budget {
                        if budget_is_override || payload.get("thinking_budget_tokens").is_none() {
                            payload["thinking_budget_tokens"] = Value::Number(budget.into());
                        }
                    }
                }
            }
            "mlx_toggle" => {
                if let Some(effort) = effort {
                    let enabled = match effort {
                        ReasoningEffort::Off => false,
                        ReasoningEffort::On => true,
                        _ => return Err("mlx_toggle supports only off and on".to_owned()),
                    };
                    set_chat_template_thinking(&mut payload, enabled, effort_is_override)?;
                    if !enabled
                        && (effort_is_override || payload.get("thinking_token_budget").is_none())
                    {
                        payload["thinking_token_budget"] = Value::Number(0.into());
                    }
                }
                if effort != Some(ReasoningEffort::Off) {
                    if let Some(budget) = budget {
                        if budget_is_override || payload.get("thinking_token_budget").is_none() {
                            payload["thinking_token_budget"] = Value::Number(budget.into());
                        }
                    }
                }
            }
            "mtplx" => {
                if let Some(effort) = effort {
                    let enabled = effort != ReasoningEffort::Off;
                    set_top_level_bool(
                        &mut payload,
                        "enable_thinking",
                        enabled,
                        effort_is_override,
                    )?;
                    if !matches!(effort, ReasoningEffort::Off | ReasoningEffort::On) {
                        set_reasoning_effort(
                            &mut payload,
                            request_path,
                            effort,
                            effort_is_override,
                        )?;
                    }
                }
                if budget.is_some() {
                    return Err(
                        "mtplx does not support a request-level reasoning budget".to_owned()
                    );
                }
            }
            "muse_system_prompt" => {
                if let Some(effort) = effort {
                    apply_muse_reasoning_strength(
                        &mut payload,
                        request_path,
                        effort,
                        effort_is_override,
                    )?;
                }
                if budget.is_some() {
                    return Err("Muse Glimmer does not support a reasoning-token budget".to_owned());
                }
            }
            adapter => return Err(format!("unsupported thinking adapter '{adapter}'")),
        }
    }

    if let Some(temperature) = controls.temperature.as_ref() {
        let selected = inference_override.temperature.or(temperature.default);
        if let Some(selected) = selected {
            if inference_override.temperature.is_some() || payload.get("temperature").is_none() {
                let number = serde_json::Number::from_f64(f64::from(selected))
                    .ok_or_else(|| "temperature must be a finite number".to_owned())?;
                payload["temperature"] = Value::Number(number);
            }
        }
    }
    serde_json::to_vec(&payload)
        .map_err(|error| format!("failed to apply inference controls: {error}"))
}

fn apply_muse_reasoning_strength(
    payload: &mut Value,
    request_path: &str,
    effort: ReasoningEffort,
    force: bool,
) -> Result<(), String> {
    let strength = match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        _ => {
            return Err(
                "Muse Glimmer supports only low, medium, high, and xhigh reasoning".to_owned(),
            )
        }
    };
    let directive = format!("Reasoning strength: {strength}");
    let path = request_path
        .trim_start_matches('/')
        .strip_prefix("v1/")
        .unwrap_or(request_path.trim_start_matches('/'));
    let root = payload
        .as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_owned())?;

    match path {
        "chat/completions" => {
            let messages = root
                .get_mut("messages")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "messages must be a JSON array".to_owned())?;
            if let Some(system) = messages
                .iter_mut()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            {
                let content = system
                    .get_mut("content")
                    .ok_or_else(|| "system message must contain content".to_owned())?;
                upsert_reasoning_directive(content, &directive, force)?;
            } else {
                messages.insert(
                    0,
                    serde_json::json!({ "role": "system", "content": directive }),
                );
            }
        }
        "responses" => {
            if let Some(instructions) = root.get_mut("instructions") {
                upsert_reasoning_directive(instructions, &directive, force)?;
            } else {
                root.insert("instructions".to_owned(), Value::String(directive));
            }
        }
        "messages" => {
            if let Some(system) = root.get_mut("system") {
                upsert_reasoning_directive(system, &directive, force)?;
            } else {
                root.insert("system".to_owned(), Value::String(directive));
            }
        }
        "completions" => {
            let prompt = root
                .get_mut("prompt")
                .ok_or_else(|| "prompt is required for completions".to_owned())?;
            upsert_reasoning_directive(prompt, &directive, force)?;
        }
        _ => return Err(format!("unsupported generation path '{request_path}'")),
    }
    Ok(())
}

fn upsert_reasoning_directive(
    content: &mut Value,
    directive: &str,
    force: bool,
) -> Result<(), String> {
    match content {
        Value::String(text) => {
            upsert_reasoning_directive_text(text, directive, force);
            Ok(())
        }
        Value::Array(blocks) => {
            let mut found = false;
            for block in blocks.iter_mut() {
                let Some(text) = block
                    .get_mut("text")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                if has_reasoning_directive(&text) {
                    found = true;
                    if force {
                        let target = block
                            .get_mut("text")
                            .and_then(|value| value.as_str())
                            .expect("text was read above");
                        let mut updated = target.to_owned();
                        upsert_reasoning_directive_text(&mut updated, directive, true);
                        block["text"] = Value::String(updated);
                    }
                }
            }
            if !found {
                blocks.insert(0, serde_json::json!({ "type": "text", "text": directive }));
            }
            Ok(())
        }
        _ => Err("system content must be a string or text-block array".to_owned()),
    }
}

fn has_reasoning_directive(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .to_ascii_lowercase()
            .starts_with("reasoning strength:")
    })
}

fn upsert_reasoning_directive_text(text: &mut String, directive: &str, force: bool) {
    if has_reasoning_directive(text) {
        if !force {
            return;
        }
        let mut replaced = false;
        *text = text
            .lines()
            .filter_map(|line| {
                if line
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("reasoning strength:")
                {
                    if replaced {
                        None
                    } else {
                        replaced = true;
                        Some(directive)
                    }
                } else {
                    Some(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    } else if text.is_empty() {
        text.push_str(directive);
    } else {
        *text = format!("{directive}\n\n{text}");
    }
}

fn set_chat_template_thinking(
    payload: &mut Value,
    enabled: bool,
    force: bool,
) -> Result<(), String> {
    let root = payload
        .as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_owned())?;
    let kwargs = root
        .entry("chat_template_kwargs")
        .or_insert_with(|| serde_json::json!({}));
    let kwargs = kwargs
        .as_object_mut()
        .ok_or_else(|| "chat_template_kwargs must be a JSON object".to_owned())?;
    if force || !kwargs.contains_key("enable_thinking") {
        kwargs.insert("enable_thinking".to_owned(), Value::Bool(enabled));
    }
    Ok(())
}

fn set_top_level_bool(
    payload: &mut Value,
    field: &str,
    value: bool,
    force: bool,
) -> Result<(), String> {
    let root = payload
        .as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_owned())?;
    if force || !root.contains_key(field) {
        root.insert(field.to_owned(), Value::Bool(value));
    }
    Ok(())
}

fn set_reasoning_effort(
    payload: &mut Value,
    request_path: &str,
    effort: ReasoningEffort,
    force: bool,
) -> Result<(), String> {
    let effort_name = match effort {
        ReasoningEffort::Off => "none",
        ReasoningEffort::On => "low",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    };
    if request_path == "responses" {
        let root = payload
            .as_object_mut()
            .ok_or_else(|| "request body must be a JSON object".to_owned())?;
        let reasoning = root
            .entry("reasoning")
            .or_insert_with(|| serde_json::json!({}));
        let reasoning = reasoning
            .as_object_mut()
            .ok_or_else(|| "reasoning must be a JSON object".to_owned())?;
        if force || !reasoning.contains_key("effort") {
            reasoning.insert("effort".to_owned(), Value::String(effort_name.to_owned()));
        }
    } else {
        let root = payload
            .as_object_mut()
            .ok_or_else(|| "request body must be a JSON object".to_owned())?;
        if force || !root.contains_key("reasoning_effort") {
            root.insert(
                "reasoning_effort".to_owned(),
                Value::String(effort_name.to_owned()),
            );
        }
    }
    Ok(())
}

fn clear_reasoning_effort(payload: &mut Value, request_path: &str) -> Result<(), String> {
    let root = payload
        .as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_owned())?;
    if request_path == "responses" {
        if let Some(reasoning) = root.get_mut("reasoning") {
            let reasoning = reasoning
                .as_object_mut()
                .ok_or_else(|| "reasoning must be a JSON object".to_owned())?;
            reasoning.remove("effort");
            if reasoning.is_empty() {
                root.remove("reasoning");
            }
        }
    } else {
        root.remove("reasoning_effort");
    }
    Ok(())
}

async fn local_upstream_endpoint(
    state: &ProxyState,
    model_id: &str,
    path_and_query: &str,
) -> Result<String, String> {
    state.fleet.require_local_available()?;
    if let Some(endpoint) = state
        .llama_swap
        .ready_upstream_endpoint(model_id, path_and_query)
        .await?
    {
        return Ok(endpoint);
    }

    let outcome = state.llama_swap.load_model(model_id, false).await?;
    if outcome.state == ControlState::Conflict {
        return Err(outcome.message);
    }
    state
        .llama_swap
        .ready_upstream_endpoint(model_id, path_and_query)
        .await?
        .ok_or_else(|| format!("{model_id} did not expose a ready upstream endpoint"))
}

fn client_selected_model<'a>(
    snapshot: &'a crate::domain::FleetSnapshot,
    client: &str,
) -> Result<Option<&'a str>, String> {
    match client {
        "hermes" => Ok(snapshot.hermes.selected_model.as_deref()),
        "openbot" => Ok(snapshot.openbot.selected_model.as_deref()),
        "opencode" => Ok(snapshot.opencode.selected_model.as_deref()),
        "codex" => Ok(snapshot.codex.selected_model.as_deref()),
        "hermes_cli" => Ok(snapshot.hermes_cli.selected_model.as_deref()),
        "claude_code" => Ok(snapshot.claude_code.selected_model.as_deref()),
        "pi" => Ok(snapshot.pi.selected_model.as_deref()),
        "copilot" => Ok(snapshot.copilot.selected_model.as_deref()),
        "vscode" => Ok(snapshot.vscode.selected_model.as_deref()),
        _ => Err(format!("unknown Agent Relay client route: {client}")),
    }
}

fn rewrite_model(body: &[u8]) -> Result<(Vec<u8>, String, String), String> {
    let mut payload: Value = serde_json::from_slice(body)
        .map_err(|error| format!("request body must be JSON: {error}"))?;
    let qualified = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "request body must contain a string model field".to_owned())?;
    let (host_id, model_id) = qualified
        .split_once('/')
        .filter(|(host, model)| !host.is_empty() && !model.is_empty())
        .ok_or_else(|| "model must use the form <host>/<profile>".to_owned())?;
    let host_id = host_id.to_owned();
    let model_id = model_id.to_owned();
    payload["model"] = Value::String(model_id.clone());
    let rewritten = serde_json::to_vec(&payload)
        .map_err(|error| format!("failed to rewrite request body: {error}"))?;
    Ok((rewritten, host_id, model_id))
}

fn rewrite_model_to_route(
    body: &[u8],
    selected_model: &str,
) -> Result<(Vec<u8>, String, String), String> {
    let (host_id, model_id) = selected_model
        .split_once('/')
        .filter(|(host, model)| !host.is_empty() && !model.is_empty())
        .ok_or_else(|| "selected route must use the form <host>/<profile>".to_owned())?;
    let mut payload: Value = serde_json::from_slice(body)
        .map_err(|error| format!("request body must be JSON: {error}"))?;
    if !payload.get("model").is_some_and(Value::is_string) {
        return Err("request body must contain a string model field".to_owned());
    }
    payload["model"] = Value::String(model_id.to_owned());
    let rewritten = serde_json::to_vec(&payload)
        .map_err(|error| format!("failed to rewrite request body: {error}"))?;
    Ok((rewritten, host_id.to_owned(), model_id.to_owned()))
}

async fn wait_reclaimed(cancelled: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(receiver) = cancelled {
        loop {
            if *receiver.borrow() {
                return;
            }
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
    std::future::pending::<()>().await;
}

pub(crate) async fn forward_buffered(
    client: &reqwest::Client,
    method: Method,
    headers: HeaderMap,
    endpoint: String,
    body: Vec<u8>,
    mut observer: Option<GenerationObserver>,
) -> Response {
    let request = copy_request_headers(client.request(method, endpoint), &headers).body(body);
    let mut cancelled = observer
        .as_ref()
        .and_then(|o| o.slot.as_ref().map(|s| s.cancellation()));
    let response = tokio::select! {
        biased;
        _ = wait_reclaimed(&mut cancelled) => {
            if let Some(observer) = observer.take() { observer.fail("host_reclaimed"); }
            return background_busy("host_reclaimed: OCR request cancelled for interactive work; retry later".into());
        }
        response = request.send() => response,
    };
    match response {
        Ok(response) => streamed_response(response, observer),
        Err(error) => {
            if let Some(observer) = observer.take() {
                observer.fail("upstream_error");
            }
            openai_error(StatusCode::BAD_GATEWAY, "upstream_error", error)
        }
    }
}

pub(crate) async fn forward_streaming(
    client: &reqwest::Client,
    method: Method,
    headers: HeaderMap,
    endpoint: String,
    body: Body,
    observer: Option<GenerationObserver>,
) -> Response {
    // The peer API receives the request after the fleet proxy has already
    // rewritten its JSON body. Re-streaming it through reqwest loses its known
    // size and causes reqwest to use chunked transfer encoding. Some upstreams
    // (including mlx_lm.server) require Content-Length and reject that request
    // with HTTP 411. Buffer the bounded inference payload so reqwest can author
    // the correct Content-Length; streamed_response still forwards generation
    // output incrementally.
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body.to_vec(),
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request", error),
    };
    forward_buffered(client, method, headers, endpoint, body, observer).await
}

fn copy_request_headers(
    mut request: reqwest::RequestBuilder,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        if !is_hop_by_hop(name)
            && name != header::CONTENT_LENGTH
            && name != header::HOST
            && name != "x-agentrelay-session"
        {
            request = request.header(name, value);
        }
    }
    request
}

fn streamed_response(
    upstream: reqwest::Response,
    observer: Option<GenerationObserver>,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut upstream_stream = upstream.bytes_stream();
    let stream = async_stream::stream! {
        let mut observer = observer;
        let mut cancelled = observer.as_ref().and_then(|o| o.slot.as_ref().map(|s| s.cancellation()));
        loop {
            let item = tokio::select! {
                biased;
                _ = wait_reclaimed(&mut cancelled) => {
                    if let Some(observer) = observer.take() { observer.fail("host_reclaimed"); }
                    // HTTP headers may already be sent: terminate with a stream error,
                    // never pretend a truncated page completed successfully.
                    yield Err(std::io::Error::other("host_reclaimed: background request cancelled; retry later"));
                    break;
                }
                item = upstream_stream.next() => item,
            };
            let Some(item) = item else { break; };
            if let (Some(observer), Ok(bytes)) = (&mut observer, &item) { observer.observe(bytes); }
            yield item.map_err(std::io::Error::other);
        }
        if let Some(observer) = observer {
            observer.finish(status.as_u16());
        }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    for (name, value) in &headers {
        if !is_hop_by_hop(name) && name != header::CONTENT_LENGTH {
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

pub(crate) struct GenerationObserver {
    _active_request: ActiveGenerationRequest<'static>,
    fleet: SharedFleetService,
    host_id: String,
    started: Instant,
    first_output_at: Option<Instant>,
    body: Vec<u8>,
    telemetry: Option<SharedTelemetry>,
    model_id: String,
    client: String,
    recorded: bool,
    slot: Option<crate::admission::SlotLease>,
    reservation: Option<AutoReservation>,
    client_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl GenerationObserver {
    pub(crate) fn hold_slot(&mut self, slot: Option<crate::admission::SlotLease>) {
        self.slot = slot;
    }
    pub(crate) fn try_new(fleet: SharedFleetService, host_id: String) -> Result<Self, String> {
        Ok(Self {
            _active_request: GENERATION_GATE.try_track().ok_or_else(|| {
                "Agent Relay is draining generation requests for shutdown".to_owned()
            })?,
            fleet,
            host_id,
            started: Instant::now(),
            first_output_at: None,
            body: Vec::new(),
            telemetry: None,
            model_id: "unknown".into(),
            client: "peer".into(),
            recorded: false,
            slot: None,
            reservation: None,
            client_permit: None,
        })
    }

    pub(crate) fn try_new_tracked(
        fleet: SharedFleetService,
        telemetry: SharedTelemetry,
        host_id: String,
        model_id: String,
        client: String,
    ) -> Result<Self, String> {
        let mut observer = Self::try_new(fleet, host_id)?;
        observer.telemetry = Some(telemetry);
        observer.model_id = model_id;
        observer.client = client;
        Ok(observer)
    }

    fn observe(&mut self, bytes: &[u8]) {
        const MAX_TELEMETRY_BODY: usize = 2 * 1024 * 1024;
        let remaining = MAX_TELEMETRY_BODY.saturating_sub(self.body.len());
        self.body
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if self.first_output_at.is_none() && contains_generation_output(&self.body) {
            self.first_output_at = Some(Instant::now());
        }
    }

    fn finish(mut self, status: u16) {
        let finished = Instant::now();
        let metrics = generation_metrics(&self.body, finished.duration_since(self.started));
        if let Some(tokens_per_second) = metrics.tokens_per_second {
            self.fleet.record_generation_throughput(
                &self.host_id,
                self.first_output_at.unwrap_or(self.started),
                finished,
                tokens_per_second,
            );
        }
        let outcome = if (200..300).contains(&status) {
            "success".to_owned()
        } else {
            format!("http_{status}")
        };
        self.recorded = true;
        self.record(outcome, finished, metrics);
    }

    fn fail(mut self, outcome: &str) {
        let finished = Instant::now();
        self.recorded = true;
        self.record(outcome.to_owned(), finished, GenerationMetrics::default());
    }

    fn record(&self, outcome: String, finished: Instant, metrics: GenerationMetrics) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        telemetry.record_request(RequestTelemetry {
            completed_at_ms: now_ms(),
            host_id: self.host_id.clone(),
            model_id: self.model_id.clone(),
            client: self.client.clone(),
            outcome,
            duration_ms: finished.duration_since(self.started).as_millis() as u64,
            ttft_ms: self
                .first_output_at
                .map(|first| first.duration_since(self.started).as_millis() as u64),
            prompt_tokens: metrics.prompt_tokens,
            output_tokens: metrics.output_tokens,
            tokens_per_second: metrics.tokens_per_second,
        });
    }
}

impl Drop for GenerationObserver {
    fn drop(&mut self) {
        if self.recorded || self.telemetry.is_none() {
            return;
        }
        self.record(
            "cancelled".into(),
            Instant::now(),
            GenerationMetrics::default(),
        );
        self.recorded = true;
    }
}

struct GenerationGate {
    state: AtomicU64,
}

impl GenerationGate {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn active_requests(&self) -> u32 {
        (self.state.load(Ordering::Acquire) & GENERATION_COUNT_MASK) as u32
    }

    fn try_track(&self) -> Option<ActiveGenerationRequest<'_>> {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if current & GENERATION_DRAINING != 0 {
                return None;
            }
            let count = current & GENERATION_COUNT_MASK;
            assert!(
                count < GENERATION_COUNT_MASK,
                "generation request counter overflow"
            );
            match self.state.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(ActiveGenerationRequest { gate: self }),
                Err(updated) => current = updated,
            }
        }
    }

    fn begin_drain(&self) -> Result<GenerationDrain<'_>, u32> {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if current & GENERATION_DRAINING != 0 {
                return Err((current & GENERATION_COUNT_MASK) as u32);
            }
            match self.state.compare_exchange_weak(
                current,
                current | GENERATION_DRAINING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) => {
                    return Ok(GenerationDrain {
                        gate: self,
                        committed: false,
                        active_requests: (previous & GENERATION_COUNT_MASK) as u32,
                    })
                }
                Err(updated) => current = updated,
            }
        }
    }
}

pub(crate) struct ActiveGenerationRequest<'a> {
    gate: &'a GenerationGate,
}

impl Drop for ActiveGenerationRequest<'_> {
    fn drop(&mut self) {
        let previous = self.gate.state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous & GENERATION_COUNT_MASK > 0,
            "active generation request counter underflow"
        );
    }
}

pub(crate) struct GenerationDrain<'a> {
    gate: &'a GenerationGate,
    committed: bool,
    active_requests: u32,
}

impl GenerationDrain<'_> {
    pub(crate) fn active_requests(&self) -> u32 {
        self.active_requests
    }

    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for GenerationDrain<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.gate
                .state
                .fetch_and(GENERATION_COUNT_MASK, Ordering::Release);
        }
    }
}

fn contains_generation_output(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        return value_contains_generation_output(&value);
    }
    text.lines().any(|line| {
        line.trim()
            .strip_prefix("data:")
            .map(str::trim)
            .filter(|data| *data != "[DONE]")
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .is_some_and(|value| value_contains_generation_output(&value))
    })
}

fn value_contains_generation_output(value: &Value) -> bool {
    if value.get("delta").is_some_and(|delta| {
        ["text", "thinking", "partial_json"].iter().any(|field| {
            delta
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
        })
    }) {
        return true;
    }

    if value
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content.iter().any(|block| {
                ["text", "thinking"].iter().any(|field| {
                    block
                        .get(field)
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                })
            })
        })
    {
        return true;
    }

    if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.ends_with(".delta"))
        && value
            .get("delta")
            .and_then(Value::as_str)
            .is_some_and(|delta| !delta.is_empty())
    {
        return true;
    }

    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                ["delta", "message"].iter().any(|container| {
                    choice.get(container).is_some_and(|output| {
                        ["content", "reasoning_content"].iter().any(|field| {
                            output
                                .get(field)
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                        }) || output
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_some_and(|calls| !calls.is_empty())
                    })
                }) || choice
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            })
        })
}

pub(crate) fn is_generation_path(path: &str) -> bool {
    matches!(
        path.trim_start_matches('/')
            .strip_prefix("v1/")
            .unwrap_or(path.trim_start_matches('/')),
        "chat/completions" | "completions" | "responses" | "messages"
    )
}

#[derive(Default)]
struct GenerationMetrics {
    prompt_tokens: Option<u64>,
    output_tokens: Option<u64>,
    tokens_per_second: Option<f32>,
}

fn generation_metrics(body: &[u8], elapsed: Duration) -> GenerationMetrics {
    let text = String::from_utf8_lossy(body);
    let mut explicit = None;
    let mut prompt_tokens = None;
    let mut completion_tokens = None;

    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        collect_generation_metrics(
            &value,
            &mut explicit,
            &mut prompt_tokens,
            &mut completion_tokens,
        );
    }
    for line in text.lines() {
        let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            collect_generation_metrics(
                &value,
                &mut explicit,
                &mut prompt_tokens,
                &mut completion_tokens,
            );
        }
    }

    let tokens_per_second = explicit.or_else(|| {
        let seconds = elapsed.as_secs_f32();
        completion_tokens
            .filter(|tokens| *tokens > 0 && seconds > 0.0)
            .map(|tokens| tokens as f32 / seconds)
    });
    GenerationMetrics {
        prompt_tokens,
        output_tokens: completion_tokens,
        tokens_per_second,
    }
}

#[cfg(test)]
fn generation_tokens_per_second(body: &[u8], elapsed: Duration) -> Option<f32> {
    generation_metrics(body, elapsed).tokens_per_second
}

fn collect_generation_metrics(
    value: &Value,
    explicit: &mut Option<f32>,
    prompt_tokens: &mut Option<u64>,
    completion_tokens: &mut Option<u64>,
) {
    let timing = value
        .get("timings")
        .and_then(|timings| {
            timings
                .get("predicted_per_second")
                .or_else(|| timings.get("tokens_per_second"))
        })
        .and_then(Value::as_f64)
        .map(|value| value as f32)
        .filter(|value| value.is_finite() && *value > 0.0);
    if timing.is_some() {
        *explicit = timing;
    }

    let usage = value.get("usage");
    let input_tokens = usage
        .and_then(|usage| {
            usage
                .get("prompt_tokens")
                .or_else(|| usage.get("input_tokens"))
        })
        .and_then(Value::as_u64);
    if let Some(tokens) = input_tokens {
        *prompt_tokens = Some(prompt_tokens.unwrap_or(0).max(tokens));
    }
    let tokens = usage
        .and_then(|usage| {
            usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens"))
        })
        .and_then(Value::as_u64);
    if let Some(tokens) = tokens {
        *completion_tokens = Some(completion_tokens.unwrap_or(0).max(tokens));
    }
}

fn is_hop_by_hop(name: &header::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn openai_error(
    status: StatusCode,
    code: &'static str,
    message: impl std::fmt::Display,
) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "code": code,
                "message": message.to_string(),
                "type": "agent_relay_error"
            }
        })),
    )
        .into_response()
}

/// Retryable admission error; no inference has been dispatched.
pub(crate) fn background_busy(error: String) -> Response {
    let code = if error.starts_with("host_occupied:") {
        "host_occupied"
    } else {
        "host_reclaimed"
    };
    let mut response = openai_error(StatusCode::SERVICE_UNAVAILABLE, code, error);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Duration as StdDuration,
    };

    use super::*;

    fn reasoning_profile() -> ModelProfile {
        ModelProfile {
            priority: Default::default(),
            id: "qwen".into(),
            display_name: "Qwen".into(),
            runtime: "llama.cpp".into(),
            kind: WorkloadKind::Text,
            capabilities: vec![ProfileCapability::Chat],
            lifecycle_adapter: "llama_swap".into(),
            resource_pool: "gpu0".into(),
            context_length: Some(65_536),
            parallel_slots: None,
            inference_controls: crate::domain::InferenceControls {
                thinking: Some(crate::domain::ThinkingControls {
                    adapter: "llama_cpp".into(),
                    efforts: vec![
                        ReasoningEffort::Off,
                        ReasoningEffort::Low,
                        ReasoningEffort::Xhigh,
                    ],
                    default_effort: Some(ReasoningEffort::Low),
                    budget_min: Some(-1),
                    budget_max: Some(16_384),
                    budget_step: Some(256),
                    default_budget: Some(-1),
                }),
                temperature: Some(crate::domain::TemperatureControls {
                    min: 0.0,
                    max: 2.0,
                    step: 0.05,
                    default: Some(0.3),
                }),
            },
        }
    }

    #[tokio::test]
    async fn one_at_a_time_client_queues_its_second_request_until_the_first_finishes() {
        use crate::{config::FleetConfig, fleet::FleetService, routing::ClientPolicy};
        let fleet = std::sync::Arc::new(
            FleetService::new(
                FleetConfig::defaults("WORKSTATION"),
                std::env::temp_dir().join(format!("relay-client-limit-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        let openbot = ClientPolicy {
            max_concurrent: Some(1),
            ..ClientPolicy::default()
        };
        let long = Duration::from_secs(5);
        let first = acquire_client_slot(&fleet, &openbot, "openbot", "chat/completions", long)
            .await
            .unwrap();
        assert!(first.is_some());
        // Listing models never queues behind a running reply.
        assert!(
            acquire_client_slot(&fleet, &openbot, "openbot", "models", long)
                .await
                .unwrap()
                .is_none()
        );
        // The second generation request waits (visible in the tray queue) instead of routing.
        let second = tokio::spawn({
            let fleet = fleet.clone();
            let openbot = openbot.clone();
            async move {
                acquire_client_slot(&fleet, &openbot, "openbot", "chat/completions", long)
                    .await
                    .map(|p| p.is_some())
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while fleet.routing_waiting.snapshot().get("openbot") != Some(&1) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!second.is_finished());
        // Unlimited clients are unaffected.
        assert!(acquire_client_slot(
            &fleet,
            &ClientPolicy::default(),
            "hermes",
            "chat/completions",
            long
        )
        .await
        .unwrap()
        .is_none());
        drop(first);
        assert_eq!(second.await.unwrap(), Ok(true));
        assert!(fleet.routing_waiting.snapshot().is_empty());
        // A request that waits too long gets a retryable 429, never a second route.
        let held = acquire_client_slot(&fleet, &openbot, "openbot", "responses", long)
            .await
            .unwrap();
        let refused = acquire_client_slot(
            &fleet,
            &openbot,
            "openbot",
            "responses",
            Duration::from_millis(30),
        )
        .await
        .map_err(client_busy)
        .unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.headers()[header::RETRY_AFTER], "5");
        drop(held);
    }

    #[test]
    fn max_concurrent_is_optional_and_bounded() {
        let mut config = crate::routing::RoutingConfig::default();
        config.clients.insert(
            "openbot".into(),
            serde_json::from_value(serde_json::json!({"mode":"auto","max_concurrent":1})).unwrap(),
        );
        assert!(config.validate().is_ok());
        assert_eq!(config.policy("openbot").max_concurrent, Some(1));
        assert_eq!(config.policy("hermes").max_concurrent, None);
        config.clients.get_mut("openbot").unwrap().max_concurrent = Some(0);
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn reclaimed_request_returns_503_before_headers_and_errors_after_headers() {
        use axum::{routing::get, Router};
        let app = Router::new()
            .route(
                "/waiting",
                get(|| async { std::future::pending::<String>().await }),
            )
            .route(
                "/stream",
                get(|| async {
                    Response::new(Body::from_stream(futures_util::stream::pending::<
                        Result<axum::body::Bytes, std::io::Error>,
                    >()))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let fleet = std::sync::Arc::new(
            crate::fleet::FleetService::new(
                crate::config::FleetConfig::defaults("test"),
                "config".into(),
            )
            .unwrap(),
        );
        let pool = std::sync::Arc::new(crate::admission::Admissions::default());
        let mut observer = GenerationObserver::try_new(fleet.clone(), "test".into()).unwrap();
        observer.hold_slot(Some(pool.track_checked("ocr", true, true).unwrap()));
        let response = tokio::spawn(async move {
            forward_buffered(
                &reqwest::Client::new(),
                Method::GET,
                HeaderMap::new(),
                format!("http://{address}/waiting"),
                vec![],
                Some(observer),
            )
            .await
        });
        pool.cancel_model("ocr");
        let response = tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
            "host_reclaimed"
        );
        let mut observer = GenerationObserver::try_new(fleet, "test".into()).unwrap();
        observer.hold_slot(Some(pool.track_checked("ocr", true, true).unwrap()));
        let response = forward_buffered(
            &reqwest::Client::new(),
            Method::GET,
            HeaderMap::new(),
            format!("http://{address}/stream"),
            vec![],
            Some(observer),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        pool.cancel_model("ocr");
        assert!(tokio::time::timeout(
            Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), 4096)
        )
        .await
        .unwrap()
        .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn background_rejection_is_retryable_and_does_not_fabricate_completion() {
        let response = background_busy("capacity_busy: reserved for interactive use".into());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "host_reclaimed");
        assert!(value.get("choices").is_none());
    }

    #[test]
    fn applies_model_defaults_and_explicit_inference_overrides() {
        let defaults = apply_inference_controls(
            "chat/completions",
            br#"{"model":"qwen","messages":[]}"#,
            &reasoning_profile(),
            &InferenceOverrides::default(),
        )
        .expect("apply defaults");
        let defaults: Value = serde_json::from_slice(&defaults).expect("decode defaults");
        assert_eq!(defaults["reasoning_effort"], "low");
        assert_eq!(defaults["thinking_budget_tokens"], -1);
        assert!((defaults["temperature"].as_f64().unwrap() - 0.3).abs() < 0.000_001);

        let overridden = apply_inference_controls(
            "responses",
            br#"{"model":"qwen","messages":[],"temperature":1.0}"#,
            &reasoning_profile(),
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Off),
                reasoning_budget: Some(4096),
                temperature: Some(0.55),
            },
        )
        .expect("apply override");
        let overridden: Value = serde_json::from_slice(&overridden).expect("decode override");
        assert!(overridden.get("reasoning").is_none());
        assert_eq!(overridden["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(overridden["thinking_budget_tokens"], 0);
        assert!((overridden["temperature"].as_f64().unwrap() - 0.55).abs() < 0.000_001);
    }

    #[test]
    fn strata_controls_use_native_budget_and_supported_efforts() {
        let mut profile = reasoning_profile();
        let thinking = profile.inference_controls.thinking.as_mut().unwrap();
        thinking.adapter = "strata".into();
        thinking.efforts = vec![
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ];
        thinking.default_budget = Some(1024);
        thinking.budget_min = Some(0);
        let apply = |body: &[u8], overrides: InferenceOverrides| -> Value {
            serde_json::from_slice(
                &apply_inference_controls("chat/completions", body, &profile, &overrides).unwrap(),
            )
            .unwrap()
        };
        let defaults = apply(br#"{"messages":[]}"#, InferenceOverrides::default());
        assert_eq!(defaults["reasoning_effort"], "low");
        assert_eq!(defaults["reasoning_budget_tokens"], 1024);
        assert!(defaults.get("thinking_budget_tokens").is_none());
        let off = apply(
            br#"{"reasoning_effort":"high"}"#,
            InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Off),
                ..Default::default()
            },
        );
        assert_eq!(off["reasoning_effort"], "none");
        let preserved = apply(
            br#"{"reasoning_effort":"medium","reasoning_budget_tokens":512}"#,
            InferenceOverrides::default(),
        );
        assert_eq!(preserved["reasoning_budget_tokens"], 512);
        assert_eq!(preserved["reasoning_effort"], "medium");
        let high = apply(
            br#"{"chat_template_kwargs":{"enable_thinking":false}}"#,
            InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::High),
                reasoning_budget: Some(2048),
                ..Default::default()
            },
        );
        assert_eq!(high["reasoning_effort"], "high");
        assert_eq!(high["reasoning_budget_tokens"], 2048);
        assert!(high
            .pointer("/chat_template_kwargs/enable_thinking")
            .is_none());
    }

    #[test]
    fn llama_cpp_off_omits_qwen_incompatible_none_effort() {
        let body = apply_inference_controls(
            "chat/completions",
            br#"{"model":"qwen","messages":[],"reasoning_effort":"xhigh"}"#,
            &reasoning_profile(),
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Off),
                ..InferenceOverrides::default()
            },
        )
        .expect("disable Qwen thinking");
        let body: Value = serde_json::from_slice(&body).expect("decode body");
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["thinking_budget_tokens"], 0);
    }

    fn toggle_profile(adapter: &str) -> ModelProfile {
        let mut profile = reasoning_profile();
        profile.inference_controls.thinking = Some(crate::domain::ThinkingControls {
            adapter: adapter.into(),
            efforts: vec![ReasoningEffort::Off, ReasoningEffort::On],
            default_effort: Some(ReasoningEffort::On),
            budget_min: (adapter != "mtplx").then_some(-1),
            budget_max: (adapter != "mtplx").then_some(16_384),
            budget_step: (adapter != "mtplx").then_some(256),
            default_budget: (adapter != "mtplx").then_some(1024),
        });
        profile
    }

    #[test]
    fn applies_honest_toggle_controls_for_llama_cpp_and_mlx() {
        let llama = apply_inference_controls(
            "chat/completions",
            br#"{"model":"ornith","messages":[]}"#,
            &toggle_profile("llama_cpp_toggle"),
            &InferenceOverrides::default(),
        )
        .expect("apply llama.cpp toggle");
        let llama: Value = serde_json::from_slice(&llama).expect("decode llama.cpp toggle");
        assert_eq!(llama["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(llama["thinking_budget_tokens"], 1024);
        assert!(llama.get("reasoning_effort").is_none());

        let mlx = apply_inference_controls(
            "chat/completions",
            br#"{"model":"gemma","messages":[]}"#,
            &toggle_profile("mlx_toggle"),
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Off),
                ..InferenceOverrides::default()
            },
        )
        .expect("apply MLX toggle");
        let mlx: Value = serde_json::from_slice(&mlx).expect("decode MLX toggle");
        assert_eq!(mlx["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(mlx["thinking_token_budget"], 0);
        assert!(mlx.get("thinking_budget_tokens").is_none());
    }

    #[test]
    fn applies_mtplx_toggle_and_native_effort_without_inventing_a_budget() {
        let mut profile = toggle_profile("mtplx");
        profile
            .inference_controls
            .thinking
            .as_mut()
            .expect("thinking")
            .efforts = vec![
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::Xhigh,
        ];
        let body = apply_inference_controls(
            "chat/completions",
            br#"{"model":"qwen","messages":[]}"#,
            &profile,
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Medium),
                ..InferenceOverrides::default()
            },
        )
        .expect("apply MTPLX effort");
        let body: Value = serde_json::from_slice(&body).expect("decode MTPLX effort");
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("thinking_budget_tokens").is_none());
        assert!(body.get("thinking_token_budget").is_none());
    }

    fn muse_profile() -> ModelProfile {
        let mut profile = reasoning_profile();
        profile.id = "muse-glimmer".into();
        profile.display_name = "Muse Glimmer".into();
        profile.inference_controls.thinking = Some(crate::domain::ThinkingControls {
            adapter: "muse_system_prompt".into(),
            efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
            ],
            default_effort: Some(ReasoningEffort::Low),
            budget_min: None,
            budget_max: None,
            budget_step: None,
            default_budget: None,
        });
        profile
    }

    #[test]
    fn applies_muse_reasoning_strength_to_supported_api_shapes() {
        let profile = muse_profile();
        let chat = apply_inference_controls(
            "chat/completions",
            br#"{"messages":[{"role":"user","content":"hello"}]}"#,
            &profile,
            &InferenceOverrides::default(),
        )
        .expect("apply Muse chat default");
        let chat: Value = serde_json::from_slice(&chat).expect("decode chat");
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "Reasoning strength: low");

        let responses = apply_inference_controls(
            "responses",
            br#"{"instructions":"Be concise","input":"hello"}"#,
            &profile,
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::High),
                ..InferenceOverrides::default()
            },
        )
        .expect("apply Muse responses override");
        let responses: Value = serde_json::from_slice(&responses).expect("decode responses");
        assert_eq!(
            responses["instructions"],
            "Reasoning strength: high\n\nBe concise"
        );

        let anthropic = apply_inference_controls(
            "messages",
            br#"{"system":[{"type":"text","text":"Be concise"}],"messages":[]}"#,
            &profile,
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Xhigh),
                ..InferenceOverrides::default()
            },
        )
        .expect("apply Muse Anthropic override");
        let anthropic: Value = serde_json::from_slice(&anthropic).expect("decode messages");
        assert_eq!(anthropic["system"][0]["text"], "Reasoning strength: xhigh");

        let completions = apply_inference_controls(
            "completions",
            br#"{"prompt":"hello"}"#,
            &profile,
            &InferenceOverrides::default(),
        )
        .expect("apply Muse completion default");
        let completions: Value = serde_json::from_slice(&completions).expect("decode completions");
        assert_eq!(completions["prompt"], "Reasoning strength: low\n\nhello");
    }

    #[test]
    fn muse_override_replaces_an_existing_directive_without_duplication() {
        let body = apply_inference_controls(
            "chat/completions",
            br#"{"messages":[{"role":"system","content":"Reasoning strength: medium\nBe concise"}]}"#,
            &muse_profile(),
            &InferenceOverrides {
                reasoning_effort: Some(ReasoningEffort::Xhigh),
                ..InferenceOverrides::default()
            },
        )
        .expect("replace Muse directive");
        let body: Value = serde_json::from_slice(&body).expect("decode body");
        assert_eq!(
            body["messages"][0]["content"],
            "Reasoning strength: xhigh\nBe concise"
        );
    }

    #[test]
    fn worker_proxy_exposes_only_health_and_versioned_api() {
        assert!(worker_path_allowed("health", &Method::GET));
        assert!(worker_path_allowed("v1/vision/caption", &Method::POST));
        assert!(worker_path_allowed("v1/vision/segment", &Method::POST));
        assert!(worker_path_allowed("v1/models", &Method::GET));
        assert!(!worker_path_allowed("health", &Method::POST));
        assert!(!worker_path_allowed("health/extra", &Method::GET));
        assert!(!worker_path_allowed("v1", &Method::POST));
        assert!(!worker_path_allowed("v1/", &Method::POST));
        assert!(!worker_path_allowed("v1/vision/caption", &Method::DELETE));
        assert!(!worker_path_allowed("docs", &Method::GET));
        assert!(!worker_path_allowed("admin/shutdown", &Method::POST));
    }

    #[test]
    fn comfy_proxy_exposes_only_bounded_workflow_routes() {
        assert!(comfy_path_allowed("prompt", &Method::POST));
        assert!(comfy_path_allowed("history/job-1", &Method::GET));
        assert!(comfy_path_allowed("view", &Method::GET));
        assert!(comfy_path_allowed("free", &Method::POST));
        assert!(!comfy_path_allowed("userdata/secrets", &Method::GET));
        assert!(!comfy_path_allowed("view", &Method::POST));
        assert!(!comfy_path_allowed("ws", &Method::GET));
    }

    #[test]
    fn rewrites_only_the_qualified_model() {
        let (body, host, model) = rewrite_model(
            br#"{"model":"air-m4/qwen/7b-mlx","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("rewrite model");
        let payload: Value = serde_json::from_slice(&body).expect("parse rewritten body");
        assert_eq!(host, "air-m4");
        assert_eq!(model, "qwen/7b-mlx");
        assert_eq!(payload["model"], "qwen/7b-mlx");
        assert_eq!(payload["messages"][0]["content"], "hi");
    }

    #[test]
    fn rejects_unqualified_model_ids() {
        let error = rewrite_model(br#"{"model":"qwen"}"#).expect_err("reject model");
        assert!(error.contains("<host>/<profile>"));
    }

    #[test]
    fn client_route_replaces_the_virtual_model_with_its_selected_target() {
        let (body, host, model) = rewrite_model_to_route(
            br#"{"model":"agentrelay","messages":[{"role":"user","content":"hi"}]}"#,
            "m1-pro/ornith1.5-35b-moe-q4",
        )
        .expect("rewrite routed model");
        let payload: Value = serde_json::from_slice(&body).expect("parse rewritten body");
        assert_eq!(host, "m1-pro");
        assert_eq!(model, "ornith1.5-35b-moe-q4");
        assert_eq!(payload["model"], "ornith1.5-35b-moe-q4");
        assert_eq!(payload["messages"][0]["content"], "hi");
    }

    #[test]
    fn client_route_base_url_is_stable_across_target_changes() {
        assert_eq!(
            client_proxy_base_url("http://127.0.0.1:38475/", "opencode"),
            "http://127.0.0.1:38475/clients/opencode/v1"
        );
    }

    #[test]
    fn codex_instructions_are_consolidated_without_promoting_user_text() {
        let body = br#"{"instructions":"base","input":[{"role":"developer","content":[{"type":"input_text","text":"policy"}]},{"role":"user","content":"untrusted"},{"role":"system","content":"update"},{"type":"function_call_output","call_id":"1","output":"result"}],"stream":true}"#;
        let result: Value =
            serde_json::from_slice(&normalize_codex_instructions(body).unwrap()).unwrap();
        assert_eq!(result["instructions"], "base\n\npolicy\n\nupdate");
        assert_eq!(result["input"].as_array().unwrap().len(), 2);
        assert_eq!(result["input"][0]["content"], "untrusted");
        assert_eq!(result["input"][1]["call_id"], "1");
        assert_eq!(result["stream"], true);
        assert!(normalize_codex_instructions(
            br#"{"input":[{"role":"developer","content":[{"type":"input_image"}]}]}"#
        )
        .is_err());
        let simple = br#"{"input":"hello"}"#;
        assert_eq!(normalize_codex_instructions(simple).unwrap(), simple);
    }

    #[test]
    fn codex_route_follows_selection_and_preserves_responses_input() {
        let mut snapshot = crate::domain::FleetSnapshot {
            local_host_id: "workstation".into(),
            config_path: String::new(),
            proxy_endpoint: String::new(),
            refreshed_at_ms: 0,
            peer_api: Default::default(),
            hosts: vec![],
            opencode: Default::default(),
            hermes: Default::default(),
            hermes_cli: Default::default(),
            openbot: Default::default(),
            codex: Default::default(),
            claude_code: Default::default(),
            pi: Default::default(),
            copilot: Default::default(),
            vscode: Default::default(),
        };
        assert_eq!(client_selected_model(&snapshot, "codex").unwrap(), None);
        for target in ["workstation/qwen", "m1-pro/ornith"] {
            snapshot.codex.selected_model = Some(target.into());
            let selected = client_selected_model(&snapshot, "codex").unwrap().unwrap();
            assert_eq!(selected, target);
            let (body, _, _) = rewrite_model_to_route(
                br#"{"model":"agentrelay","input":"hello","stream":true}"#,
                selected,
            )
            .unwrap();
            let payload: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(payload["input"], "hello");
            assert_eq!(payload["stream"], true);
        }
        assert!(client_selected_model(&snapshot, "unknown").is_err());
    }

    #[test]
    fn openbot_tool_round_trip_preserves_protocol_and_independent_route() {
        let snapshot: crate::domain::FleetSnapshot = serde_json::from_value(serde_json::json!({
            "local_host_id":"local", "config_path":"", "proxy_endpoint":"", "refreshed_at_ms":0,
            "peer_api": {"state":"starting", "listen_address":null, "error":null}, "hosts":[],
            "opencode": crate::domain::OpenCodeStatus::default(),
            "hermes": crate::domain::HermesStatus::default(),
            "hermes_cli": crate::domain::HermesStatus::default(),
            "openbot": crate::domain::HarnessStatus { selected_model: Some("m1-pro/test".into()), ..Default::default() },
            "codex": crate::domain::HarnessStatus::default(), "claude_code": crate::domain::HarnessStatus::default(),
            "pi": crate::domain::HarnessStatus::default(), "copilot": crate::domain::HarnessStatus::default(),
            "vscode": crate::domain::HarnessStatus::default()
        })).unwrap();
        assert_eq!(client_selected_model(&snapshot, "hermes").unwrap(), None);
        assert_eq!(client_selected_model(&snapshot, "opencode").unwrap(), None);
        let target = client_selected_model(&snapshot, "openbot")
            .unwrap()
            .unwrap();
        let mut payload = serde_json::json!({"model":"agentrelay", "stream":true,
            "tools":[{"type":"function","function":{"name":"echo","parameters":{"type":"object"}}}],
            "messages":[{"role":"user","content":"echo hello"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"echo","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"hello"}]});
        let (body, host, model) =
            rewrite_model_to_route(&serde_json::to_vec(&payload).unwrap(), target).unwrap();
        assert_eq!((host.as_str(), model.as_str()), ("m1-pro", "test"));
        payload["model"] = Value::String("test".into());
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), payload);
        assert_eq!(
            client_proxy_base_url("http://127.0.0.1:38475", "openbot"),
            "http://127.0.0.1:38475/clients/openbot/v1"
        );
    }

    #[test]
    fn session_affinity_is_bounded_and_never_forwarded_to_the_runtime() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-agentrelay-session",
            HeaderValue::from_static("synthetic-session"),
        );
        assert_eq!(session_affinity(&headers), session_affinity(&headers));
        assert!(session_affinity(&headers).is_some());
        let request =
            copy_request_headers(reqwest::Client::new().post("http://localhost/"), &headers)
                .build()
                .unwrap();
        assert!(!request.headers().contains_key("x-agentrelay-session"));
        headers.insert(
            "x-agentrelay-session",
            HeaderValue::from_str(&"x".repeat(129)).unwrap(),
        );
        assert!(session_affinity(&headers).is_none());
    }

    #[test]
    fn openbot_stream_delivers_tool_delta_before_completion_and_cancels_upstream() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let delta = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"echo\",\"arguments\":\"{}\"}}]}}]}\n\n";
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(StdDuration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(socket.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            write!(socket, "{:x}\r\n", delta.len()).unwrap();
            socket.write_all(delta).unwrap();
            socket.write_all(b"\r\n").unwrap();
            socket.flush().unwrap();
            // Deliberately do not finish: the client must receive the delta now,
            // and dropping its response must tear down this still-active stream.
            let result = socket.read(&mut byte);
            closed_tx
                .send(
                    matches!(result, Ok(0))
                        || result.is_err_and(|e| {
                            matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            )
                        }),
                )
                .unwrap();
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let pool = std::sync::Arc::new(crate::admission::Admissions::default());
            let fleet = std::sync::Arc::new(
                crate::fleet::FleetService::new(
                    crate::config::FleetConfig::defaults("test"),
                    "config".into(),
                )
                .unwrap(),
            );
            let mut observer = GenerationObserver::try_new(fleet, "test".into()).unwrap();
            observer.hold_slot(Some(pool.reserve("qwen", 1, 0).unwrap()));
            let response = forward_buffered(
                &reqwest::Client::new(),
                Method::GET,
                HeaderMap::new(),
                endpoint,
                vec![],
                Some(observer),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let mut stream = response.into_body().into_data_stream();
            let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(first.as_ref(), delta);
            assert!(pool.reserve("qwen", 1, 0).is_err());
            drop(stream);
            assert!(pool.reserve("qwen", 1, 0).is_ok());
            tokio::time::sleep(Duration::from_millis(30)).await;
        });
        assert!(
            closed_rx.recv_timeout(StdDuration::from_secs(6)).unwrap(),
            "upstream was not cancelled"
        );
        server.join().unwrap();
    }

    #[test]
    fn peer_forwarding_authors_content_length_instead_of_chunked_encoding() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (request_tx, request_rx) = mpsc::channel();
        let upstream = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept request");
            socket
                .set_read_timeout(Some(StdDuration::from_secs(5)))
                .expect("set read timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let read = socket.read(&mut chunk).expect("read request");
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(boundary) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header_end = boundary + 4;
                    let headers = String::from_utf8_lossy(&request[..boundary]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .expect("forwarded Content-Length header");
                    assert!(
                        !headers.to_ascii_lowercase().contains("transfer-encoding:"),
                        "fixed request must not use chunked transfer encoding"
                    );
                    break (header_end, content_length);
                }
            };
            while request.len() - header_end < content_length {
                let read = socket.read(&mut chunk).expect("read request body");
                assert!(read > 0, "request ended before its declared body length");
                request.extend_from_slice(&chunk[..read]);
            }
            request_tx
                .send((content_length, request[header_end..].to_vec()))
                .expect("report captured request");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .expect("write response");
        });

        let payload = br#"{"model":"ornith1.5-35b-moe-q4","messages":[]}"#.to_vec();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&payload.len().to_string()).expect("content length"),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let response = runtime.block_on(forward_streaming(
            &reqwest::Client::new(),
            Method::POST,
            headers,
            endpoint,
            Body::from(payload.clone()),
            None,
        ));
        assert_eq!(response.status(), StatusCode::OK);

        let (declared_length, forwarded_body) = request_rx
            .recv_timeout(StdDuration::from_secs(5))
            .expect("capture forwarded request");
        assert_eq!(declared_length, payload.len());
        assert_eq!(forwarded_body, payload);
        upstream.join().expect("upstream thread");
    }

    #[test]
    fn reads_llama_cpp_throughput_from_a_streaming_chunk() {
        let body = b"data: {\"choices\":[],\"timings\":{\"predicted_per_second\":31.25}}\n\ndata: [DONE]\n\n";
        assert_eq!(
            generation_tokens_per_second(body, Duration::from_secs(5)),
            Some(31.25)
        );
    }

    #[test]
    fn estimates_throughput_from_openai_usage_when_runtime_timings_are_absent() {
        let body = br#"{"usage":{"completion_tokens":40}}"#;
        assert_eq!(
            generation_tokens_per_second(body, Duration::from_secs(2)),
            Some(20.0)
        );
    }

    #[test]
    fn recognizes_first_generated_delta_but_not_role_or_keepalive_chunks() {
        assert!(!contains_generation_output(
            b": keepalive\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n"
        ));
        assert!(contains_generation_output(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n"
        ));
    }

    #[test]
    fn recognizes_anthropic_content_deltas_and_usage() {
        let body = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\n";
        assert!(contains_generation_output(body));
        assert_eq!(
            generation_tokens_per_second(body, Duration::from_secs(2)),
            Some(10.0)
        );
    }

    #[test]
    fn recognizes_only_generation_routes() {
        assert!(is_generation_path("chat/completions"));
        assert!(is_generation_path("v1/responses"));
        assert!(is_generation_path("messages"));
        assert!(!is_generation_path("models"));
    }

    #[test]
    fn hermes_bridge_cors_accepts_only_packaged_or_loopback_origins() {
        for origin in [
            "null",
            "tauri://localhost",
            "http://localhost:1420",
            "https://127.0.0.1:8443",
            "http://[::1]:3000",
        ] {
            assert!(
                trusted_hermes_origin(&HeaderValue::from_str(origin).expect("valid origin")),
                "expected {origin} to be trusted"
            );
        }
        for origin in [
            "https://example.com",
            "http://localhost.evil.example",
            "https://127.0.0.1.evil.example",
            "http://localhost:not-a-port",
        ] {
            assert!(
                !trusted_hermes_origin(&HeaderValue::from_str(origin).expect("valid origin")),
                "expected {origin} to be rejected"
            );
        }
    }

    #[test]
    fn active_generation_guard_counts_and_releases_on_drop() {
        let gate = GenerationGate::new();
        {
            let first = gate.try_track().expect("accept first request");
            assert_eq!(gate.active_requests(), 1);
            {
                let second = gate.try_track().expect("accept second request");
                assert_eq!(gate.active_requests(), 2);
                drop(second);
            }
            assert_eq!(gate.active_requests(), 1);
            drop(first);
        }
        assert_eq!(gate.active_requests(), 0);
    }

    #[test]
    fn generation_drain_linearizes_new_requests_and_reopens_when_abandoned() {
        let gate = GenerationGate::new();
        let active = gate.try_track().expect("accept active request");
        {
            let drain = gate.begin_drain().expect("begin drain");
            assert_eq!(drain.active_requests(), 1);
            assert!(gate.try_track().is_none());
        }
        assert!(gate.try_track().is_some());
        drop(active);
    }

    #[test]
    fn committed_generation_drain_keeps_intake_closed() {
        let gate = GenerationGate::new();
        gate.begin_drain().expect("begin drain").commit();
        assert!(gate.try_track().is_none());
    }
}
