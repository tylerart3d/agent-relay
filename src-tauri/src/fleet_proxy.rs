use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
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
    channels::{
        parse_channel_message, ChannelAdapterHeartbeat, ChannelAddress, ChannelCommand,
        ChannelCommandRequest, ChannelDeliveryRequest, ChannelHandoffStatus, ChannelHarness,
        ChannelNativeArchiveStatus, ChannelRouteTarget, HarnessDeliveryRequest,
        HarnessSessionArchiveRequest, ParsedChannelMessage, SharedChannelAdapterRegistry,
        SharedChannelRouteStore,
    },
    config,
    domain::{
        ConnectionState, ControlOutcome, ControlState, FleetLoadModelRequest,
        FleetUnloadModelsRequest, InferenceOverrides, LoadModelRequest, ModelProfile,
        ProfileCapability, ReasoningEffort, UnloadModelsRequest, WorkloadKind,
    },
    fleet::SharedFleetService,
    gateway::{GatewayHeartbeat, SharedGatewayCoordinator},
    hermes::SharedHermesIntegration,
    hermes_bridge::{HermesPresence, HermesSwitchAck, SharedHermesBridge},
    llama_swap::SharedLlamaSwapSupervisor,
    opencode::SharedOpenCodeIntegration,
    pi_runner::SharedPiRunner,
    telemetry::{now_ms, RequestTelemetry, SharedTelemetry},
};

const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
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

#[derive(Clone)]
struct ProxyState {
    fleet: SharedFleetService,
    llama_swap: SharedLlamaSwapSupervisor,
    hermes_bridge: SharedHermesBridge,
    hermes: SharedHermesIntegration,
    opencode: SharedOpenCodeIntegration,
    pi: SharedPiRunner,
    client: reqwest::Client,
    route_store: SharedChannelRouteStore,
    adapter_registry: SharedChannelAdapterRegistry,
    gateway: SharedGatewayCoordinator,
    telemetry: SharedTelemetry,
    attach_choosers: SharedAttachChoosers,
}

#[derive(Clone)]
pub(crate) struct ProxyIntegrations {
    pub(crate) hermes_bridge: SharedHermesBridge,
    pub(crate) hermes: SharedHermesIntegration,
    pub(crate) opencode: SharedOpenCodeIntegration,
    pub(crate) pi: SharedPiRunner,
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

#[derive(Clone, Copy)]
enum ChannelRouteAction {
    Use,
    New,
    Move,
    Resume,
}

const ATTACH_CHOOSER_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_ATTACH_CHOICES: usize = 8;

#[derive(Clone)]
struct AttachConversationChoice {
    harness_host_id: String,
    host_display_name: String,
    native_session_id: String,
    title: String,
    project_name: String,
    directory: String,
    updated_at_ms: u64,
    model_host_id: String,
    model_display_name: String,
    model_host_display_name: String,
    model_id: String,
    model_loaded: bool,
}

struct AttachChooser {
    expires_at: Instant,
    conversation_label: Option<String>,
    choices: Vec<AttachConversationChoice>,
}

type SharedAttachChoosers = Arc<Mutex<HashMap<String, AttachChooser>>>;

pub async fn serve(
    fleet: SharedFleetService,
    integrations: ProxyIntegrations,
    llama_swap: SharedLlamaSwapSupervisor,
    channel_routes: SharedChannelRouteStore,
    channel_adapters: SharedChannelAdapterRegistry,
    gateway: SharedGatewayCoordinator,
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
        .route("/api/v1/control/load", post(fleet_load_model))
        .route("/api/v1/control/unload", post(fleet_unload_models))
        .route("/api/v1/channels/routes", get(list_channel_routes))
        .route("/api/v1/channels/adapters", get(list_channel_adapters))
        .route(
            "/api/v1/channels/adapters/heartbeat",
            post(channel_adapter_heartbeat),
        )
        .route(
            "/api/v1/channels/gateway/decision",
            get(channel_gateway_decision),
        )
        .route(
            "/api/v1/channels/gateway/heartbeat",
            post(channel_gateway_heartbeat),
        )
        .route("/api/v1/channels/command", post(channel_command))
        .route("/api/v1/channels/deliver", post(deliver_channel_message))
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
        .with_state(ProxyState {
            fleet,
            llama_swap,
            hermes_bridge: integrations.hermes_bridge,
            hermes: integrations.hermes,
            opencode: integrations.opencode,
            pi: integrations.pi,
            client,
            route_store: channel_routes,
            adapter_registry: channel_adapters,
            gateway,
            telemetry,
            attach_choosers: Arc::new(Mutex::new(HashMap::new())),
        });
    let listener = tokio::net::TcpListener::bind(&listen_address)
        .await
        .map_err(|error| format!("failed to bind fleet proxy to {listen_address}: {error}"))?;

    axum::serve(listener, app)
        .await
        .map_err(|error| format!("fleet proxy stopped: {error}"))
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
        match local_model_endpoint(&state, &model_id, &path_and_query).await {
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
        match local_model_endpoint(&state, &model_id, &path_and_query).await {
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

async fn list_channel_routes(State(state): State<ProxyState>) -> Json<Value> {
    Json(serde_json::json!({
        "ok": true,
        "routes": state.route_store.list(),
    }))
}

async fn list_channel_adapters(State(state): State<ProxyState>) -> Json<Value> {
    Json(serde_json::json!({
        "ok": true,
        "adapters": state.adapter_registry.list(),
    }))
}

async fn channel_adapter_heartbeat(
    State(state): State<ProxyState>,
    Json(heartbeat): Json<ChannelAdapterHeartbeat>,
) -> Response {
    match state.adapter_registry.heartbeat(heartbeat) {
        Ok(adapter) => Json(serde_json::json!({ "ok": true, "adapter": adapter })).into_response(),
        Err(error) => channel_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn channel_gateway_decision(State(state): State<ProxyState>) -> Json<Value> {
    Json(serde_json::json!({
        "ok": true,
        "decision": state.gateway.decision(&state.fleet.snapshot()),
    }))
}

async fn channel_gateway_heartbeat(
    State(state): State<ProxyState>,
    Json(heartbeat): Json<GatewayHeartbeat>,
) -> Json<Value> {
    let status = state.gateway.heartbeat(heartbeat);
    state
        .fleet
        .update_channel_gateway_status(Some(status.clone()));
    Json(serde_json::json!({ "ok": true, "gateway": status }))
}

fn channel_address_key(address: &ChannelAddress) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        address.channel, address.account_id, address.conversation_id
    )
}

fn channel_command_reply(command: &str, message: impl Into<String>) -> Response {
    Json(serde_json::json!({
        "ok": true,
        "handled": true,
        "command": command,
        "message": message.into(),
    }))
    .into_response()
}

async fn with_mobile_message(response: Response, message: String) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let status = response.status();
    let Ok(bytes) = to_bytes(response.into_body(), 1024 * 1024).await else {
        return channel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to format the mobile route response",
        );
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(&bytes) else {
        return channel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decode the mobile route response",
        );
    };
    payload["mobile_message"] = Value::String(message);
    (status, Json(payload)).into_response()
}

fn concise_label(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let shortened = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn project_label(choice: &AttachConversationChoice) -> String {
    if !choice.project_name.trim().is_empty() {
        return choice.project_name.clone();
    }
    choice
        .directory
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(&choice.directory)
        .to_owned()
}

fn format_conversation_choices(choices: &[AttachConversationChoice]) -> String {
    let options = choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            format!(
                "{}. {} — {} ({} · {} on {}{})",
                index + 1,
                concise_label(&project_label(choice), 28),
                concise_label(&choice.title, 42),
                choice.host_display_name,
                concise_label(&choice.model_display_name, 32),
                choice.model_host_display_name,
                if choice.model_loaded { "" } else { ", idle" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Choose an OpenCode conversation:\n{options}\n\nReply with 1-{}, or !ar cancel.",
        choices.len()
    )
}

fn numbered_choice(text: &str, choice_count: usize) -> Option<usize> {
    let number = text.trim().parse::<usize>().ok()?;
    if (1..=choice_count).contains(&number) {
        Some(number - 1)
    } else {
        None
    }
}

async fn start_attach_chooser(state: &ProxyState, request: &ChannelCommandRequest) -> Response {
    let snapshot = state.fleet.refresh().await;
    let mut choices = Vec::new();
    for host in snapshot
        .hosts
        .iter()
        .filter(|host| host.connection != ConnectionState::Offline)
    {
        let sessions = if state.fleet.is_local_host(&host.id) {
            state.opencode.list_sessions()
        } else {
            state.fleet.request_peer_opencode_sessions(&host.id).await
        };
        let Ok(sessions) = sessions else {
            continue;
        };
        choices.extend(
            sessions
                .into_iter()
                .filter(|session| !session.archived)
                .filter_map(|session| {
                    let (model_host_id, model_id) =
                        session.relay_model.as_deref()?.split_once('/')?;
                    let model_host = snapshot.hosts.iter().find(|peer| {
                        peer.id == model_host_id && peer.connection != ConnectionState::Offline
                    })?;
                    let profile = model_host.models.iter().find(|profile| {
                        profile.id == model_id && profile.supports_text_inference()
                    })?;
                    Some(AttachConversationChoice {
                        harness_host_id: host.id.clone(),
                        host_display_name: host.display_name.clone(),
                        native_session_id: session.id,
                        title: session.title,
                        project_name: session.project_name,
                        directory: session.directory,
                        updated_at_ms: session.updated_at_ms,
                        model_host_id: model_host.id.clone(),
                        model_host_display_name: model_host.display_name.clone(),
                        model_id: profile.id.clone(),
                        model_display_name: profile.display_name.clone(),
                        model_loaded: model_host.loaded_model_id.as_deref() == Some(model_id),
                    })
                }),
        );
    }
    choices.sort_by_key(|choice| std::cmp::Reverse(choice.updated_at_ms));
    choices.truncate(MAX_ATTACH_CHOICES);
    if choices.is_empty() {
        return channel_command_reply(
            "attach",
            "No Agent Relay OpenCode conversations with an available model route were found.",
        );
    }
    let message = format_conversation_choices(&choices);
    state
        .attach_choosers
        .lock()
        .expect("attach choosers poisoned")
        .insert(
            channel_address_key(&request.address),
            AttachChooser {
                expires_at: Instant::now() + ATTACH_CHOOSER_TTL,
                conversation_label: request.conversation_label.clone(),
                choices,
            },
        );
    channel_command_reply("attach", message)
}

async fn continue_attach_chooser(
    state: &ProxyState,
    request: &ChannelCommandRequest,
) -> Option<Response> {
    let key = channel_address_key(&request.address);
    let chooser = state
        .attach_choosers
        .lock()
        .expect("attach choosers poisoned")
        .remove(&key)?;
    if Instant::now() > chooser.expires_at {
        return Some(channel_command_reply(
            "attach",
            "That chooser expired. Send !ar attach to start again.",
        ));
    }

    let choices = chooser.choices;
    let Some(index) = numbered_choice(&request.text, choices.len()) else {
        let message = format!(
            "Please reply with a number from 1 to {}, or send !ar cancel.",
            choices.len()
        );
        state
            .attach_choosers
            .lock()
            .expect("attach choosers poisoned")
            .insert(
                key,
                chooser_with_conversations(chooser.conversation_label, choices),
            );
        return Some(channel_command_reply("attach", message));
    };
    let conversation = choices[index].clone();
    let snapshot = state.fleet.refresh().await;
    let model_is_still_available = snapshot.hosts.iter().any(|host| {
        host.id == conversation.model_host_id
            && host.connection != ConnectionState::Offline
            && host
                .models
                .iter()
                .any(|profile| profile.id == conversation.model_id)
    });
    if !model_is_still_available {
        return Some(channel_command_reply(
            "attach",
            "That conversation's model is no longer available. Send !ar attach to refresh the list.",
        ));
    }
    let action = if state.route_store.get(&request.address).is_some() {
        ChannelRouteAction::Move
    } else {
        ChannelRouteAction::New
    };
    let mobile_message = format!(
        "Attached to “{}” in OpenCode on {}, using its {} model on {}. Send your next message to continue; an idle model will reload automatically.",
        concise_label(&conversation.title, 48),
        conversation.host_display_name,
        conversation.model_display_name,
        conversation.model_host_display_name
    );
    let response = activate_channel_route(
        state,
        request.address.clone(),
        chooser.conversation_label,
        action,
        None,
        ChannelHarness::OpenCode,
        Some(conversation.harness_host_id),
        conversation.model_host_id,
        conversation.model_id,
        Some(conversation.directory),
        Some(conversation.native_session_id),
        false,
        false,
    )
    .await;
    Some(with_mobile_message(response, mobile_message).await)
}

fn chooser_with_conversations(
    conversation_label: Option<String>,
    choices: Vec<AttachConversationChoice>,
) -> AttachChooser {
    AttachChooser {
        expires_at: Instant::now() + ATTACH_CHOOSER_TTL,
        conversation_label,
        choices,
    }
}

async fn channel_command(
    State(state): State<ProxyState>,
    Json(request): Json<ChannelCommandRequest>,
) -> Response {
    if let Err(error) = request.address.validate() {
        return channel_error(StatusCode::BAD_REQUEST, error);
    }
    let parsed = match parse_channel_message(&request.text) {
        Ok(parsed) => parsed,
        Err(error) => return channel_error(StatusCode::BAD_REQUEST, error),
    };
    let ParsedChannelMessage::Command(command) = parsed else {
        if let Some(response) = continue_attach_chooser(&state, &request).await {
            return response;
        }
        let route = state.route_store.get(&request.address);
        return Json(serde_json::json!({
            "ok": true,
            "handled": false,
            "message": "ordinary channel message",
            "route": route,
        }))
        .into_response();
    };

    match command {
        ChannelCommand::Help => Json(serde_json::json!({
            "ok": true,
            "handled": true,
            "command": "help",
            "message": "Mobile commands: !ar attach; !ar route; !ar recent; !ar cancel. Advanced commands remain available: !ar hosts; !ar models; !ar sessions; !ar use; !ar new; !ar move; !ar resume; !ar unload.",
        }))
        .into_response(),
        ChannelCommand::Attach => start_attach_chooser(&state, &request).await,
        ChannelCommand::Cancel => {
            let removed = state
                .attach_choosers
                .lock()
                .expect("attach choosers poisoned")
                .remove(&channel_address_key(&request.address))
                .is_some();
            channel_command_reply(
                "cancel",
                if removed {
                    "Cancelled the current Agent Relay chooser."
                } else {
                    "There is no active Agent Relay chooser."
                },
            )
        }
        ChannelCommand::Status => {
            let snapshot = state.fleet.refresh().await;
            let route = state.route_store.get(&request.address);
            let target = route.as_ref().and_then(|route| {
                snapshot.hosts.iter().find(|host| host.id == route.host_id).map(|host| {
                    serde_json::json!({
                        "host_id": host.id,
                        "host_display_name": host.display_name,
                        "connection": host.connection,
                        "loaded_model_id": host.loaded_model_id,
                        "active_requests": host.active_requests,
                        "route_model_loaded": host.loaded_model_id.as_deref() == Some(route.model_id.as_str()),
                    })
                })
            });
            Json(serde_json::json!({
                "ok": true,
                "handled": true,
                "command": "status",
                "message": if route.is_some() {
                    "conversation route found"
                } else {
                    "Photon is connected, but this conversation is not routed. Send !ar attach to choose one."
                },
                "route": route,
                "target": target,
            }))
            .into_response()
        }
        ChannelCommand::Hosts => {
            let snapshot = state.fleet.refresh().await;
            let hosts = snapshot
                .hosts
                .iter()
                .map(|host| serde_json::json!({
                    "id": host.id,
                    "display_name": host.display_name,
                    "connection": host.connection,
                    "loaded_model_id": host.loaded_model_id,
                    "active_requests": host.active_requests,
                }))
                .collect::<Vec<_>>();
            Json(serde_json::json!({
                "ok": true,
                "handled": true,
                "command": "hosts",
                "message": format!("{} fleet hosts", hosts.len()),
                "hosts": hosts,
            }))
            .into_response()
        }
        ChannelCommand::Models { host_id } => {
            let snapshot = state.fleet.refresh().await;
            if let Some(host_id) = host_id.as_deref() {
                if !snapshot.hosts.iter().any(|host| host.id == host_id) {
                    return channel_error(
                        StatusCode::NOT_FOUND,
                        format!("unknown fleet host: {host_id}"),
                    );
                }
            }
            let models = snapshot
                .hosts
                .iter()
                .filter(|host| host_id.as_ref().is_none_or(|filter| &host.id == filter))
                .flat_map(|host| {
                    host.models
                        .iter()
                        .filter(|model| model.supports_text_inference())
                        .map(move |model| serde_json::json!({
                            "host_id": host.id,
                            "host_display_name": host.display_name,
                            "connection": host.connection,
                            "id": model.id,
                            "display_name": model.display_name,
                            "runtime": model.runtime,
                            "loaded": host.loaded_model_id.as_deref() == Some(model.id.as_str()),
                        }))
                })
                .collect::<Vec<_>>();
            Json(serde_json::json!({
                "ok": true,
                "handled": true,
                "command": "models",
                "message": format!("{} compatible text models", models.len()),
                "models": models,
            }))
            .into_response()
        }
        ChannelCommand::Sessions { include_archived } => {
            let sessions = state
                .route_store
                .sessions(&request.address, include_archived);
            Json(serde_json::json!({
                "ok": true,
                "handled": true,
                "command": "sessions",
                "message": format!("{} Agent Relay sessions for this conversation", sessions.len()),
                "sessions": sessions,
            }))
            .into_response()
        }
        ChannelCommand::Resume { session_id } => {
            let Some(route) = state.route_store.get_session(&request.address, session_id) else {
                return channel_error(
                    StatusCode::NOT_FOUND,
                    format!("session #{session_id} was not found for this conversation"),
                );
            };
            activate_channel_route(
                &state,
                request.address,
                request.conversation_label,
                ChannelRouteAction::Resume,
                Some(session_id),
                route.harness,
                route.harness_host_id,
                route.host_id,
                route.model_id,
                route.project,
                route.native_session_id,
                true,
                false,
            )
            .await
        }
        ChannelCommand::Use {
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
            native_session_id,
            force,
        } => {
            let current = state.route_store.get(&request.address);
            let Some(resolved_harness) = harness.or_else(|| current.as_ref().map(|route| route.harness.clone())) else {
                return channel_error(
                    StatusCode::CONFLICT,
                    "this conversation has no harness; specify direct, hermes, opencode, or pi",
                );
            };
            let resolved_harness_host = harness_host_id.or_else(|| {
                current
                    .as_ref()
                    .filter(|route| route.harness == resolved_harness)
                    .and_then(|route| route.harness_host_id.clone())
            });
            let resolved_project = project.or_else(|| {
                current
                    .as_ref()
                    .filter(|route| route.harness == resolved_harness)
                    .and_then(|route| route.project.clone())
            });
            activate_channel_route(
                &state,
                request.address,
                request.conversation_label,
                ChannelRouteAction::Use,
                None,
                resolved_harness,
                resolved_harness_host,
                host_id,
                model_id,
                resolved_project,
                native_session_id,
                true,
                force,
            )
            .await
        }
        ChannelCommand::New {
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
            native_session_id,
            force,
        } => {
            activate_channel_route(
                &state,
                request.address,
                request.conversation_label,
                ChannelRouteAction::New,
                None,
                harness,
                harness_host_id,
                host_id,
                model_id,
                project,
                native_session_id,
                true,
                force,
            )
            .await
        }
        ChannelCommand::Move {
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
            native_session_id,
            force,
        } => {
            activate_channel_route(
                &state,
                request.address,
                request.conversation_label,
                ChannelRouteAction::Move,
                None,
                harness,
                harness_host_id,
                host_id,
                model_id,
                project,
                native_session_id,
                true,
                force,
            )
            .await
        }
        ChannelCommand::Unload { host_id, force } => {
            let snapshot = state.fleet.refresh().await;
            let Some(host) = snapshot.hosts.iter().find(|host| host.id == host_id) else {
                return channel_error(
                    StatusCode::NOT_FOUND,
                    format!("unknown fleet host: {host_id}"),
                );
            };
            if host.connection == ConnectionState::Offline {
                return channel_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("{} is offline", host.display_name),
                );
            }
            let result = if state.fleet.is_local_host(&host_id) {
                state.llama_swap.unload_models(force).await
            } else {
                state
                    .fleet
                    .request_peer_unload(&host_id, &UnloadModelsRequest { force })
                    .await
            };
            match result {
                Ok(outcome) => {
                    let status = if outcome.state == ControlState::Conflict {
                        StatusCode::CONFLICT
                    } else {
                        StatusCode::OK
                    };
                    (
                        status,
                        Json(serde_json::json!({
                            "ok": status.is_success(),
                            "handled": true,
                            "command": "unload",
                            "message": outcome.message,
                            "confirmation_required": outcome.state == ControlState::Conflict,
                            "retry_command": (outcome.state == ControlState::Conflict).then(|| format!("/ar unload {host_id} force")),
                            "result": outcome,
                        })),
                    )
                        .into_response()
                }
                Err(error) => channel_error(StatusCode::BAD_GATEWAY, error),
            }
        }
    }
}

async fn deliver_channel_message(
    State(state): State<ProxyState>,
    Json(request): Json<ChannelDeliveryRequest>,
) -> Response {
    if let Err(error) = request.address.validate() {
        return channel_error(StatusCode::BAD_REQUEST, error);
    }
    if request.text.trim().is_empty() {
        return channel_error(StatusCode::BAD_REQUEST, "message text cannot be empty");
    }
    let Some(route) = state.route_store.get(&request.address) else {
        return channel_error(
            StatusCode::CONFLICT,
            "this conversation has no active route; use /ar new first",
        );
    };

    if let Some(cached) = state.route_store.cached_exchange(
        &request.address,
        route.session_id,
        request.external_message_id.as_deref(),
    ) {
        let route = match state
            .route_store
            .complete_handoff(&request.address, route.session_id)
        {
            Ok(route) => route,
            Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
        };
        return Json(serde_json::json!({
            "ok": true,
            "handled": true,
            "reply": cached.assistant_text,
            "route": route,
            "session_mode": if route.harness == ChannelHarness::Direct { "stateless" } else { "native" },
            "replayed": true,
        }))
        .into_response();
    }

    let original_text = request.text;
    let delivery_text = state.route_store.delivery_text(&route, &original_text);

    match route.harness {
        ChannelHarness::Direct => {
            deliver_direct_message(
                &state,
                route,
                request.external_message_id,
                original_text,
                delivery_text,
            )
            .await
        }
        ChannelHarness::Hermes => {
            deliver_hermes_message(
                &state,
                route,
                request.external_message_id,
                original_text,
                delivery_text,
            )
            .await
        }
        ChannelHarness::OpenCode => {
            deliver_opencode_message(
                &state,
                route,
                request.external_message_id,
                original_text,
                delivery_text,
            )
            .await
        }
        ChannelHarness::Pi => {
            deliver_pi_message(
                &state,
                route,
                request.external_message_id,
                original_text,
                delivery_text,
            )
            .await
        }
    }
}

async fn deliver_hermes_message(
    state: &ProxyState,
    mut route: crate::channels::ChannelRoute,
    external_message_id: Option<String>,
    original_text: String,
    delivery_text: String,
) -> Response {
    let transcript_message_id = external_message_id.clone();
    let harness_host_id = route
        .harness_host_id
        .clone()
        .unwrap_or_else(|| state.fleet.local_host_id().to_owned());
    let native_session_id = match route.native_session_id.clone() {
        Some(native_session_id) => native_session_id,
        None => {
            let native_session_id = format!("agent-relay-{}", uuid::Uuid::new_v4().simple());
            route = match state.route_store.bind_native_session(
                &route.address,
                route.session_id,
                native_session_id.clone(),
            ) {
                Ok(route) => route,
                Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
            };
            native_session_id
        }
    };
    let request = HarnessDeliveryRequest {
        session_id: route.session_id,
        native_session_id: Some(native_session_id),
        idempotency_key: external_message_id
            .unwrap_or_else(|| format!("agent-relay-{}", uuid::Uuid::new_v4().simple())),
        host_id: route.host_id.clone(),
        model_id: route.model_id.clone(),
        project: None,
        text: delivery_text,
    };
    let result = if state.fleet.is_local_host(&harness_host_id) {
        let proxy_endpoint = state.fleet.snapshot().proxy_endpoint;
        state
            .hermes
            .deliver_api_message(
                &request.host_id,
                &request.model_id,
                &proxy_endpoint,
                request
                    .native_session_id
                    .as_deref()
                    .expect("Hermes delivery always has a native session"),
                &request.idempotency_key,
                &request.text,
            )
            .await
    } else {
        state
            .fleet
            .request_peer_hermes_delivery(&harness_host_id, &request)
            .await
    };
    let delivery = match result {
        Ok(delivery) => delivery,
        Err(error) => return channel_error(StatusCode::BAD_GATEWAY, error),
    };
    route = match persist_channel_exchange(
        state,
        &route,
        transcript_message_id,
        original_text,
        delivery.reply.clone(),
    )
    .await
    {
        Ok(route) => route,
        Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    Json(serde_json::json!({
        "ok": true,
        "handled": true,
        "reply": delivery.reply,
        "route": route,
        "session_mode": "native",
    }))
    .into_response()
}

async fn deliver_opencode_message(
    state: &ProxyState,
    mut route: crate::channels::ChannelRoute,
    external_message_id: Option<String>,
    original_text: String,
    delivery_text: String,
) -> Response {
    let transcript_message_id = external_message_id.clone();
    let harness_host_id = route
        .harness_host_id
        .clone()
        .unwrap_or_else(|| state.fleet.local_host_id().to_owned());
    let request = HarnessDeliveryRequest {
        session_id: route.session_id,
        native_session_id: route.native_session_id.clone(),
        idempotency_key: external_message_id
            .unwrap_or_else(|| format!("agent-relay-{}", uuid::Uuid::new_v4().simple())),
        host_id: route.host_id.clone(),
        model_id: route.model_id.clone(),
        project: route.project.clone(),
        text: delivery_text,
    };
    let result = if state.fleet.is_local_host(&harness_host_id) {
        state
            .opencode
            .deliver_api_message(&request, &state.fleet)
            .await
    } else {
        state
            .fleet
            .request_peer_harness_delivery(&harness_host_id, "opencode", &request)
            .await
    };
    let delivery = match result {
        Ok(delivery) => delivery,
        Err(error) => return channel_error(StatusCode::BAD_GATEWAY, error),
    };
    if let Some(native_session_id) = delivery.native_session_id.clone() {
        if route.native_session_id.as_deref() != Some(native_session_id.as_str()) {
            route = match state.route_store.bind_native_session(
                &route.address,
                route.session_id,
                native_session_id,
            ) {
                Ok(route) => route,
                Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
            };
        }
    }
    route = match persist_channel_exchange(
        state,
        &route,
        transcript_message_id,
        original_text,
        delivery.reply.clone(),
    )
    .await
    {
        Ok(route) => route,
        Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    Json(serde_json::json!({
        "ok": true,
        "handled": true,
        "reply": delivery.reply,
        "route": route,
        "session_mode": "native",
    }))
    .into_response()
}

async fn deliver_pi_message(
    state: &ProxyState,
    mut route: crate::channels::ChannelRoute,
    external_message_id: Option<String>,
    original_text: String,
    delivery_text: String,
) -> Response {
    let transcript_message_id = external_message_id.clone();
    let harness_host_id = route
        .harness_host_id
        .clone()
        .unwrap_or_else(|| state.fleet.local_host_id().to_owned());
    let native_session_id = match route.native_session_id.clone() {
        Some(native_session_id) => native_session_id,
        None => {
            let native_session_id = uuid::Uuid::new_v4().hyphenated().to_string();
            route = match state.route_store.bind_native_session(
                &route.address,
                route.session_id,
                native_session_id.clone(),
            ) {
                Ok(route) => route,
                Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
            };
            native_session_id
        }
    };
    let request = HarnessDeliveryRequest {
        session_id: route.session_id,
        native_session_id: Some(native_session_id),
        idempotency_key: external_message_id
            .unwrap_or_else(|| format!("agent-relay-{}", uuid::Uuid::new_v4().simple())),
        host_id: route.host_id.clone(),
        model_id: route.model_id.clone(),
        project: route.project.clone(),
        text: delivery_text,
    };
    let result = if state.fleet.is_local_host(&harness_host_id) {
        state.pi.deliver_message(&request, &state.fleet).await
    } else {
        state
            .fleet
            .request_peer_harness_delivery(&harness_host_id, "pi", &request)
            .await
    };
    let delivery = match result {
        Ok(delivery) => delivery,
        Err(error) => return channel_error(StatusCode::BAD_GATEWAY, error),
    };
    route = match persist_channel_exchange(
        state,
        &route,
        transcript_message_id,
        original_text,
        delivery.reply.clone(),
    )
    .await
    {
        Ok(route) => route,
        Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    Json(serde_json::json!({
        "ok": true,
        "handled": true,
        "reply": delivery.reply,
        "route": route,
        "session_mode": "native",
    }))
    .into_response()
}

async fn deliver_direct_message(
    state: &ProxyState,
    mut route: crate::channels::ChannelRoute,
    external_message_id: Option<String>,
    original_text: String,
    delivery_text: String,
) -> Response {
    let body = match serde_json::to_vec(&serde_json::json!({
        "model": route.model_id,
        "messages": [{"role": "user", "content": delivery_text}],
        "stream": false,
        "max_tokens": 4096,
    })) {
        Ok(body) => body,
        Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let upstream = proxy_target_request(
        state,
        TargetRequest {
            path: "chat/completions".into(),
            uri: axum::http::Uri::from_static("/v1/chat/completions"),
            method: Method::POST,
            headers,
            body,
            client: "channel-direct".into(),
        },
        route.host_id.clone(),
        route.model_id.clone(),
    )
    .await;
    let status = upstream.status();
    let bytes = match to_bytes(upstream.into_body(), MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return channel_error(StatusCode::BAD_GATEWAY, error.to_string()),
    };
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(error) => {
            return channel_error(
                StatusCode::BAD_GATEWAY,
                format!("model returned invalid JSON: {error}"),
            )
        }
    };
    if !status.is_success() {
        let error = payload
            .pointer("/error/message")
            .or_else(|| payload.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("model request failed");
        return channel_error(status, error);
    }
    let Some(reply) = openai_reply_text(&payload) else {
        return channel_error(StatusCode::BAD_GATEWAY, "model returned no text reply");
    };
    route = match persist_channel_exchange(
        state,
        &route,
        external_message_id,
        original_text,
        reply.clone(),
    )
    .await
    {
        Ok(route) => route,
        Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };

    Json(serde_json::json!({
        "ok": true,
        "handled": true,
        "reply": reply,
        "route": route,
        "session_mode": "stateless",
        "finish_reason": payload.pointer("/choices/0/finish_reason"),
        "usage": payload.get("usage"),
    }))
    .into_response()
}

async fn persist_channel_exchange(
    state: &ProxyState,
    route: &crate::channels::ChannelRoute,
    external_message_id: Option<String>,
    original_text: String,
    reply: String,
) -> Result<crate::channels::ChannelRoute, String> {
    state
        .route_store
        .record_exchange(route, external_message_id, original_text, reply)?;
    let current = state
        .route_store
        .get_session(&route.address, route.session_id)
        .ok_or_else(|| format!("session #{} was not found", route.session_id))?;
    if matches!(
        current.native_archive_status,
        Some(ChannelNativeArchiveStatus::Pending | ChannelNativeArchiveStatus::Failed)
    ) {
        if let Some(source_session_id) = current.handoff_from_session_id {
            let archive_result = match state
                .route_store
                .get_session(&current.address, source_session_id)
            {
                Some(source) => set_native_session_archived(state, &source, true).await,
                None => Err(format!("source session #{source_session_id} was not found")),
            };
            state.route_store.record_native_archive_result(
                &current.address,
                current.session_id,
                source_session_id,
                archive_result,
            )?;
        }
    }
    state
        .route_store
        .complete_handoff(&route.address, route.session_id)
}

async fn set_native_session_archived(
    state: &ProxyState,
    route: &crate::channels::ChannelRoute,
    archived: bool,
) -> Result<(), String> {
    let Some(native_session_id) = route.native_session_id.as_deref() else {
        return Ok(());
    };
    let harness_host_id = route
        .harness_host_id
        .as_deref()
        .unwrap_or_else(|| state.fleet.local_host_id());
    if state.fleet.is_local_host(harness_host_id) {
        return match route.harness {
            ChannelHarness::Hermes => state
                .hermes
                .set_session_archived(native_session_id, archived),
            ChannelHarness::OpenCode => state
                .opencode
                .set_session_archived(native_session_id, archived),
            ChannelHarness::Pi => state.pi.set_session_archived(native_session_id, archived),
            ChannelHarness::Direct => Ok(()),
        };
    }
    state
        .fleet
        .request_peer_harness_session_archive(
            harness_host_id,
            route.harness.command_name(),
            &HarnessSessionArchiveRequest {
                native_session_id: native_session_id.to_owned(),
                archived,
            },
        )
        .await
}

fn openai_reply_text(payload: &Value) -> Option<String> {
    let content = payload.pointer("/choices/0/message/content")?;
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

#[allow(clippy::too_many_arguments)]
async fn activate_channel_route(
    state: &ProxyState,
    address: ChannelAddress,
    conversation_label: Option<String>,
    action: ChannelRouteAction,
    session_id: Option<u64>,
    harness: ChannelHarness,
    harness_host_id: Option<String>,
    host_id: String,
    model_id: String,
    mut project: Option<String>,
    native_session_id: Option<String>,
    prepare_model: bool,
    force: bool,
) -> Response {
    let snapshot = state.fleet.refresh().await;
    let Some(host) = snapshot.hosts.iter().find(|host| host.id == host_id) else {
        return channel_error(
            StatusCode::NOT_FOUND,
            format!("unknown fleet host: {host_id}"),
        );
    };
    if host.connection == ConnectionState::Offline {
        return channel_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{} is offline", host.display_name),
        );
    }
    if let Some(harness_host_id) = harness_host_id.as_deref() {
        let Some(harness_host) = snapshot
            .hosts
            .iter()
            .find(|host| host.id == harness_host_id)
        else {
            return channel_error(
                StatusCode::NOT_FOUND,
                format!("unknown harness host: {harness_host_id}"),
            );
        };
        if harness_host.connection == ConnectionState::Offline {
            return channel_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("harness host {} is offline", harness_host.display_name),
            );
        }
    }
    let Some(profile) = host.models.iter().find(|profile| profile.id == model_id) else {
        return channel_error(
            StatusCode::NOT_FOUND,
            format!("{} has no profile named {model_id}", host.display_name),
        );
    };
    if !profile.supports_capability(&crate::domain::ProfileCapability::Chat) {
        return channel_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{host_id}/{model_id} is incompatible with {} messaging",
                harness.display_name()
            ),
        );
    }
    if project.is_some() && !matches!(harness, ChannelHarness::OpenCode | ChannelHarness::Pi) {
        return channel_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "projects are currently supported only for OpenCode and Pi sessions",
        );
    }

    if let Some(session_id) = native_session_id.as_deref() {
        if harness != ChannelHarness::OpenCode {
            return channel_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "existing conversations can be attached only to OpenCode routes",
            );
        }
        let session_host_id = harness_host_id
            .as_deref()
            .unwrap_or_else(|| state.fleet.local_host_id());
        let sessions = if state.fleet.is_local_host(session_host_id) {
            state.opencode.list_sessions()
        } else {
            state
                .fleet
                .request_peer_opencode_sessions(session_host_id)
                .await
        };
        let sessions = match sessions {
            Ok(sessions) => sessions,
            Err(error) => return channel_error(StatusCode::BAD_GATEWAY, error),
        };
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            return channel_error(
                StatusCode::NOT_FOUND,
                format!("OpenCode conversation {session_id} was not found on {session_host_id}"),
            );
        };
        if session.archived {
            return channel_error(
                StatusCode::CONFLICT,
                format!("OpenCode conversation {session_id} is archived"),
            );
        }
        if let Some(selected_project) = project.as_deref() {
            if selected_project != session.directory {
                return channel_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "OpenCode conversation {session_id} belongs to {}, not {selected_project}",
                        session.directory
                    ),
                );
            }
        } else {
            project = Some(session.directory.clone());
        }
    }

    let context_window = match harness {
        ChannelHarness::OpenCode => Some(state.opencode.context_window()),
        ChannelHarness::Hermes => Some(state.hermes.context_window()),
        _ => None,
    };
    let result = if !prepare_model {
        Ok(ControlOutcome {
            state: ControlState::Noop,
            host_id: host_id.clone(),
            active_requests: host.active_requests,
            loaded_model_id: host.loaded_model_id.clone(),
            message: "preserved the conversation's existing model route".into(),
        })
    } else if state.fleet.is_local_host(&host_id) {
        state
            .llama_swap
            .load_model_with_context(&model_id, force, context_window)
            .await
    } else {
        state
            .fleet
            .request_peer_load(
                &host_id,
                &LoadModelRequest {
                    model_id: model_id.clone(),
                    force,
                    context_window,
                },
            )
            .await
    };
    let command = match action {
        ChannelRouteAction::Use => "use",
        ChannelRouteAction::New => "new",
        ChannelRouteAction::Move => "move",
        ChannelRouteAction::Resume => "resume",
    };
    let outcome = match result {
        Ok(outcome) if outcome.state == ControlState::Conflict => {
            let harness_target = harness_host_id.as_ref().map_or_else(
                || harness.command_name().to_owned(),
                |host| format!("{}@{host}", harness.command_name()),
            );
            let retry_command = if matches!(action, ChannelRouteAction::Resume) {
                format!("/ar resume {}", session_id.expect("resume session ID"))
            } else {
                format!(
                    "!ar {command} {harness_target} {host_id}/{model_id}{}{} force",
                    project
                        .as_ref()
                        .map(|project| format!(" project '{project}'"))
                        .unwrap_or_default(),
                    native_session_id
                        .as_ref()
                        .map(|session| format!(" session '{session}'"))
                        .unwrap_or_default()
                )
            };
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "ok": false,
                    "handled": true,
                    "command": command,
                    "message": outcome.message,
                    "confirmation_required": true,
                    "retry_command": retry_command,
                    "result": outcome,
                })),
            )
                .into_response();
        }
        Ok(outcome) => outcome,
        Err(error) => return channel_error(StatusCode::BAD_GATEWAY, error),
    };
    state.fleet.refresh().await;

    if matches!(action, ChannelRouteAction::Resume) {
        let resume_session_id = session_id.expect("resume session ID");
        let Some(target) = state.route_store.get_session(&address, resume_session_id) else {
            return channel_error(
                StatusCode::NOT_FOUND,
                format!("session #{resume_session_id} was not found for this conversation"),
            );
        };
        if target.native_archived_at_ms.is_some() {
            if let Err(error) = set_native_session_archived(state, &target, false).await {
                return channel_error(
                    StatusCode::BAD_GATEWAY,
                    format!("could not restore the native conversation: {error}"),
                );
            }
            if let Err(error) = state
                .route_store
                .mark_native_unarchived(&address, resume_session_id)
            {
                return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error);
            }
        }
    }

    let route = match action {
        ChannelRouteAction::Use => state.route_store.set(
            address,
            ChannelRouteTarget {
                conversation_label,
                harness: harness.clone(),
                harness_host_id,
                host_id: host_id.clone(),
                model_id: model_id.clone(),
                project,
            },
        ),
        ChannelRouteAction::New => state.route_store.start_session(
            address,
            ChannelRouteTarget {
                conversation_label,
                harness: harness.clone(),
                harness_host_id,
                host_id: host_id.clone(),
                model_id: model_id.clone(),
                project,
            },
        ),
        ChannelRouteAction::Move => state.route_store.move_session(
            address,
            ChannelRouteTarget {
                conversation_label,
                harness: harness.clone(),
                harness_host_id,
                host_id: host_id.clone(),
                model_id: model_id.clone(),
                project,
            },
        ),
        ChannelRouteAction::Resume => state
            .route_store
            .resume(&address, session_id.expect("resume session ID")),
    };
    let mut route = match route {
        Ok(route) => route,
        Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if let Some(native_session_id) = native_session_id {
        route = match state.route_store.attach_existing_native_session(
            &route.address,
            route.session_id,
            native_session_id,
        ) {
            Ok(route) => route,
            Err(error) => return channel_error(StatusCode::INTERNAL_SERVER_ERROR, error),
        };
    }
    let message = match action {
        ChannelRouteAction::Use => format!(
            "Session #{} now uses {} with {host_id}/{model_id}",
            route.session_id,
            harness.display_name()
        ),
        ChannelRouteAction::New => format!(
            "Started session #{} with {} and {host_id}/{model_id}",
            route.session_id,
            harness.display_name()
        ),
        ChannelRouteAction::Move => format!(
            "Moved to session #{} with {} and {host_id}/{model_id}; conversation context will transfer with the next message",
            route.session_id,
            harness.display_name()
        ),
        ChannelRouteAction::Resume => format!(
            "Resumed session #{} with {} and {host_id}/{model_id}",
            route.session_id,
            harness.display_name()
        ),
    };
    Json(serde_json::json!({
        "ok": true,
        "handled": true,
        "command": command,
        "message": message,
        "route": route,
        "context_handoff": if route.handoff_status == Some(ChannelHandoffStatus::Pending) { "pending_first_destination_reply" } else { "not_requested" },
        "native_harness_archive": match route.native_archive_status {
            Some(ChannelNativeArchiveStatus::Pending) => "pending_first_destination_reply",
            Some(ChannelNativeArchiveStatus::Completed) => "completed",
            Some(ChannelNativeArchiveStatus::Failed) => "failed_retry_pending",
            None => "not_requested",
        },
        "result": outcome,
    }))
    .into_response()
}

fn channel_error(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({
            "ok": false,
            "handled": true,
            "error": error.into(),
        })),
    )
        .into_response()
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
                .filter(|model| model.supports_text_inference())
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
    let online = selected_model
        .and_then(|qualified| qualified.split_once('/'))
        .and_then(|(host_id, model_id)| {
            snapshot
                .hosts
                .iter()
                .find(|host| host.id == host_id)
                .map(|host| {
                    host.connection != ConnectionState::Offline
                        && host.loaded_model_id.as_deref() == Some(model_id)
                })
        })
        .unwrap_or(false);

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
    let selected_model = match client_selected_model(&snapshot, &client) {
        Ok(Some(selected_model)) => selected_model.to_owned(),
        Ok(None) => {
            return openai_error(
                StatusCode::CONFLICT,
                "route_not_selected",
                format!("choose an Agent Relay target for {client} first"),
            )
        }
        Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_client", error),
    };
    let bytes = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request", error),
    };
    let (body, host_id, model_id) = match rewrite_model_to_route(&bytes, &selected_model) {
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
            client,
        },
        host_id,
        model_id,
    )
    .await
}

async fn proxy_request(
    State(state): State<ProxyState>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
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
        },
        host_id,
        model_id,
    )
    .await
}

struct TargetRequest {
    path: String,
    uri: axum::http::Uri,
    method: Method,
    headers: HeaderMap,
    body: Vec<u8>,
    client: String,
}

async fn proxy_target_request(
    state: &ProxyState,
    request: TargetRequest,
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
    let endpoint = if state.fleet.is_local_host(&host_id) {
        match local_model_endpoint(state, &model_id, &path_and_query).await {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return openai_error(StatusCode::SERVICE_UNAVAILABLE, "model_unavailable", error)
            }
        }
    } else {
        match state.fleet.peer_proxy_endpoint(&host_id, &path_and_query) {
            Ok(endpoint) => endpoint,
            Err(error) => return openai_error(StatusCode::NOT_FOUND, "unknown_host", error),
        }
    };

    let observer = if is_generation_path(&request.path) {
        match GenerationObserver::try_new_tracked(
            state.fleet.clone(),
            state.telemetry.clone(),
            host_id,
            model_id.clone(),
            request.client,
        ) {
            Ok(observer) => Some(observer),
            Err(error) => {
                return openai_error(StatusCode::SERVICE_UNAVAILABLE, "proxy_draining", error)
            }
        }
    } else {
        None
    };
    forward_buffered(
        &state.client,
        request.method,
        request.headers,
        endpoint,
        body,
        observer,
    )
    .await
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
            "llama_cpp" => {
                if let Some(effort) = effort {
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
                    let effort_is_missing = if request_path == "responses" {
                        payload.pointer("/reasoning/effort").is_none()
                    } else {
                        payload.get("reasoning_effort").is_none()
                    };
                    if effort_is_override || effort_is_missing {
                        if request_path == "responses" {
                            payload["reasoning"] = serde_json::json!({ "effort": effort_name });
                        } else {
                            payload["reasoning_effort"] = Value::String(effort_name.into());
                        }
                    }
                    if effort == ReasoningEffort::Off
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

async fn local_model_endpoint(
    state: &ProxyState,
    model_id: &str,
    path_and_query: &str,
) -> Result<String, String> {
    if let Some(endpoint) = state
        .llama_swap
        .ready_model_endpoint(model_id, path_and_query)
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
        .ready_model_endpoint(model_id, path_and_query)
        .await?
        .ok_or_else(|| format!("{model_id} did not expose a ready inference endpoint"))
}

fn client_selected_model<'a>(
    snapshot: &'a crate::domain::FleetSnapshot,
    client: &str,
) -> Result<Option<&'a str>, String> {
    match client {
        "hermes" => Ok(snapshot.hermes.selected_model.as_deref()),
        "opencode" => Ok(snapshot.opencode.selected_model.as_deref()),
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

pub(crate) async fn forward_buffered(
    client: &reqwest::Client,
    method: Method,
    headers: HeaderMap,
    endpoint: String,
    body: Vec<u8>,
    mut observer: Option<GenerationObserver>,
) -> Response {
    let request = copy_request_headers(client.request(method, endpoint), &headers).body(body);
    match request.send().await {
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
        if !is_hop_by_hop(name) && name != header::CONTENT_LENGTH && name != header::HOST {
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
        while let Some(item) = upstream_stream.next().await {
            if let (Some(observer), Ok(bytes)) = (&mut observer, &item) {
                observer.observe(bytes);
            }
            yield item;
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
}

impl GenerationObserver {
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

struct ActiveGenerationRequest<'a> {
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
            id: "qwen".into(),
            display_name: "Qwen".into(),
            runtime: "llama.cpp".into(),
            kind: WorkloadKind::Text,
            capabilities: vec![ProfileCapability::Chat],
            lifecycle_adapter: "llama_swap".into(),
            resource_pool: "gpu0".into(),
            context_length: Some(65_536),
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
        assert_eq!(overridden["reasoning"]["effort"], "none");
        assert_eq!(overridden["thinking_budget_tokens"], 0);
        assert!((overridden["temperature"].as_f64().unwrap() - 0.55).abs() < 0.000_001);
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

    fn chooser_conversation(title: &str) -> AttachConversationChoice {
        AttachConversationChoice {
            harness_host_id: "m1-pro".into(),
            host_display_name: "M1 Pro".into(),
            native_session_id: "ses_test".into(),
            title: title.into(),
            project_name: "Tower Defense".into(),
            directory: "/Users/tester/Projects/Tower Defense".into(),
            updated_at_ms: 1,
            model_host_id: "workstation".into(),
            model_host_display_name: "WORKSTATION".into(),
            model_id: "qwen-internal-id".into(),
            model_display_name: "Qwen 3.8 Q3".into(),
            model_loaded: false,
        }
    }

    #[test]
    fn mobile_chooser_accepts_only_a_number_in_range() {
        assert_eq!(numbered_choice(" 2 ", 3), Some(1));
        assert_eq!(numbered_choice("0", 3), None);
        assert_eq!(numbered_choice("4", 3), None);
        assert_eq!(numbered_choice("two", 3), None);
    }

    #[test]
    fn mobile_chooser_messages_hide_internal_ids_and_paths() {
        let conversation = chooser_conversation("Implement enemy wave timing");
        let message = format_conversation_choices(std::slice::from_ref(&conversation));
        assert!(message.contains(
            "1. Tower Defense — Implement enemy wave timing (M1 Pro · Qwen 3.8 Q3 on WORKSTATION, idle)"
        ));
        assert!(message.contains("Reply with 1-1"));
        assert!(!message.contains("ses_test"));
        assert!(!message.contains("/Users/tester"));
        assert!(!message.contains("qwen-internal-id"));
    }

    #[test]
    fn extracts_text_from_openai_string_and_block_responses() {
        assert_eq!(
            openai_reply_text(&serde_json::json!({
                "choices": [{"message": {"content": "hello"}}]
            }))
            .as_deref(),
            Some("hello")
        );
        assert_eq!(
            openai_reply_text(&serde_json::json!({
                "choices": [{"message": {"content": [
                    {"type": "text", "text": "hello "},
                    {"type": "image", "url": "ignored"},
                    {"type": "text", "text": "world"}
                ]}}]
            }))
            .as_deref(),
            Some("hello world")
        );
        assert!(openai_reply_text(&serde_json::json!({"choices": []})).is_none());
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
