//! NVIDIA VoiceChat transport. Audio and tool events remain opaque and are never
//! logged, persisted, or executed here. Lifecycle stays with the host supervisor.
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    tungstenite::{protocol::WebSocketConfig, Message as UpstreamMessage},
    MaybeTlsStream, WebSocketStream,
};

use crate::{
    domain::{ConnectionState, ControlState, FleetSnapshot},
    fleet::SharedFleetService,
    fleet_proxy,
    llama_swap::SharedLlamaSwapSupervisor,
};

const MAX_FRAME: usize = 256 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
type VoiceError = (StatusCode, String);
type Upstream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct VoiceState {
    fleet: SharedFleetService,
    supervisor: SharedLlamaSwapSupervisor,
    peer: bool,
}

pub(crate) fn local_router<S: Clone + Send + Sync + 'static>(
    fleet: SharedFleetService,
    supervisor: SharedLlamaSwapSupervisor,
) -> Router<S> {
    Router::new()
        .route("/voice", get(page))
        .route("/voice/{asset}", get(asset))
        .route("/api/voice/models", get(models))
        .route("/api/voice/{host}/{model}/prepare", post(prepare))
        .route("/api/voice/{host}/{model}/realtime", get(realtime))
        .with_state(VoiceState {
            fleet,
            supervisor,
            peer: false,
        })
}

pub(crate) fn peer_router<S: Clone + Send + Sync + 'static>(
    fleet: SharedFleetService,
    supervisor: SharedLlamaSwapSupervisor,
) -> Router<S> {
    Router::new()
        .route("/api/v1/voice/{model}/prepare", post(peer_prepare))
        .route("/api/v1/voice/{model}/realtime", get(peer_realtime))
        .with_state(VoiceState {
            fleet,
            supervisor,
            peer: true,
        })
}

fn content(mime: &'static str, body: &'static str) -> Response {
    ([
        (header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    ], body).into_response()
}
async fn page() -> Response {
    content(
        "text/html; charset=utf-8",
        include_str!("../../voice/index.html"),
    )
}
async fn asset(Path(asset): Path<String>) -> Response {
    match asset.as_str() {
        "client.js" => content("text/javascript", include_str!("../../voice/client.js")),
        "audio.js" => content("text/javascript", include_str!("../../voice/audio.js")),
        "capture.js" => content("text/javascript", include_str!("../../voice/capture.js")),
        "style.css" => content("text/css", include_str!("../../voice/style.css")),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn models(State(state): State<VoiceState>) -> Response {
    let snapshot = state.fleet.snapshot();
    let models: Vec<_> = snapshot.hosts.iter().flat_map(|host| host.models.iter()
        .filter(|model| model.is_voice_service()).map(|model| serde_json::json!({
            "host_id":host.id, "host_name":host.display_name, "model_id":model.id,
            "name":model.display_name, "online":host.connection != ConnectionState::Offline,
            "runtime":model.runtime,
            "protocol":if model.runtime == "mlx_voicechat" { "mlx_voicechat" } else { "nemotron_realtime" }
        }))).collect();
    Json(serde_json::json!({"models":models})).into_response()
}

pub(crate) fn validate_target(
    snapshot: &FleetSnapshot,
    host: &str,
    model: &str,
) -> Result<(), VoiceError> {
    let host = snapshot
        .hosts
        .iter()
        .find(|item| item.id == host)
        .ok_or((StatusCode::NOT_FOUND, "Unknown voice host".into()))?;
    if !host.available {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Machine unavailable for new work".into(),
        ));
    }
    if host.connection == ConnectionState::Offline {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{} is offline", host.display_name),
        ));
    }
    let model = host
        .models
        .iter()
        .find(|item| item.id == model)
        .ok_or((StatusCode::NOT_FOUND, "Unknown voice profile".into()))?;
    if !model.is_voice_service() {
        return Err((
            StatusCode::BAD_REQUEST,
            "This profile does not support real-time voice".into(),
        ));
    }
    Ok(())
}

// Browsers must originate on this Relay's voice page. Peers and native clients
// omit Origin. Never forward browser cookies, authorization, or arbitrary URLs.
fn allowed_origin(headers: &HeaderMap, endpoint: &str, peer: bool) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => {
            !peer
                && origin
                    .to_str()
                    .is_ok_and(|origin| origin == endpoint.trim_end_matches('/'))
        }
    }
}
fn validate(
    state: &VoiceState,
    headers: &HeaderMap,
    host: &str,
    model: &str,
) -> Result<(), VoiceError> {
    let snapshot = state.fleet.snapshot();
    if !allowed_origin(headers, &snapshot.proxy_endpoint, state.peer) {
        return Err((
            StatusCode::FORBIDDEN,
            "Open Voice from this machine's Agent Relay tray".into(),
        ));
    }
    validate_target(&snapshot, host, model)
}
fn error((status, message): VoiceError) -> Response {
    (status, Json(serde_json::json!({"error":message}))).into_response()
}

async fn peer_prepare(
    State(state): State<VoiceState>,
    Path(model): Path<String>,
    headers: HeaderMap,
) -> Response {
    let host = state.fleet.local_host_id().to_owned();
    prepare(State(state), Path((host, model)), headers).await
}
async fn prepare(
    State(state): State<VoiceState>,
    Path((host, model)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = validate(&state, &headers, &host, &model) {
        return error(err);
    }
    if !state.fleet.is_local_host(&host) {
        let endpoint = match state.fleet.peer_voice_endpoint(&host, &model, "prepare") {
            Ok(endpoint) => endpoint,
            Err(err) => return error((StatusCode::NOT_FOUND, err)),
        };
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(620))
            .build()
        {
            Ok(client) => client,
            Err(_) => {
                return error((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not initialize voice client".into(),
                ))
            }
        };
        return match client.post(endpoint).send().await {
            Ok(response) => {
                let status = response.status();
                // Our peer only returns a small status object. Bound even a broken peer.
                let mut stream = response.bytes_stream();
                let mut body = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(chunk) = chunk else { break };
                    if body.len() + chunk.len() > 8192 {
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
            }
            Err(_) => error((
                StatusCode::BAD_GATEWAY,
                "Voice host could not be reached or did not become ready".into(),
            )),
        };
    }
    match tokio::time::timeout(Duration::from_secs(600), state.supervisor.load_model(&model, false)).await {
        Ok(Ok(outcome)) if outcome.state == ControlState::Conflict => error((StatusCode::CONFLICT, outcome.message)),
        Ok(Ok(_)) => Json(serde_json::json!({"ready":true})).into_response(),
        Ok(Err(err)) => error((StatusCode::BAD_GATEWAY, format!("Voice runtime failed: {err}. Check the voice runtime installation, model files, and available memory."))),
        Err(_) => error((StatusCode::GATEWAY_TIMEOUT, "Voice runtime did not become ready within 10 minutes".into())),
    }
}

async fn peer_realtime(
    State(state): State<VoiceState>,
    Path(model): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let host = state.fleet.local_host_id().to_owned();
    realtime(State(state), Path((host, model)), headers, ws).await
}
async fn realtime(
    State(state): State<VoiceState>,
    Path((host, model)): Path<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(err) = validate(&state, &headers, &host, &model) {
        return error(err);
    }
    let Some(generation) = fleet_proxy::track_voice_session() else {
        return error((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent Relay is restarting; reconnect shortly".into(),
        ));
    };
    let (endpoint, lease) = if state.fleet.is_local_host(&host) {
        match tokio::time::timeout(
            Duration::from_secs(600),
            state.supervisor.begin_voice_session(&model),
        )
        .await
        {
            Ok(Ok((endpoint, lease))) => (endpoint, Some(lease)),
            Ok(Err(err)) => return error((StatusCode::CONFLICT, err)),
            Err(_) => return error((StatusCode::GATEWAY_TIMEOUT, "Voice load timed out".into())),
        }
    } else {
        match state.fleet.peer_voice_endpoint(&host, &model, "realtime") {
            Ok(endpoint) => (endpoint, None),
            Err(err) => return error((StatusCode::NOT_FOUND, err)),
        }
    };
    let mut cancelled = lease.as_ref().map(|lease| lease.cancelled.clone());
    let upstream = tokio::select! {
        result = connect(&endpoint) => match result {
            Ok(upstream) => upstream,
            Err(err) => return error(err),
        },
        _ = wait_cancel(&mut cancelled) => return error((StatusCode::CONFLICT, "Voice session was cancelled".into())),
    };
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| async move {
            // Retain both guards until BOTH connections have gone away.
            let _generation = generation;
            let _lease = lease;
            bridge(socket, upstream, cancelled).await;
        })
}

async fn connect(endpoint: &str) -> Result<Upstream, VoiceError> {
    let url = endpoint
        .strip_prefix("http://")
        .map(|s| format!("ws://{s}"))
        .ok_or((
            StatusCode::BAD_GATEWAY,
            "Voice endpoint must be a configured HTTP peer or local runtime".into(),
        ))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    // Peer cold loads are explicitly prepared first; the upgrade itself must be fast.
    match tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async_with_config(url, Some(config), false)).await {
        Ok(Ok((socket, _))) => Ok(socket),
        _ => Err((StatusCode::BAD_GATEWAY, "Voice connection failed. Prepare the model first and check the runtime health endpoint.".into())),
    }
}
async fn wait_cancel(cancelled: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    match cancelled {
        Some(receiver) => {
            if !*receiver.borrow() {
                let _ = receiver.changed().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

async fn bridge(
    mut client: WebSocket,
    mut upstream: Upstream,
    mut cancelled: Option<tokio::sync::watch::Receiver<bool>>,
) {
    loop {
        tokio::select! {
            _ = wait_cancel(&mut cancelled) => break,
            message = client.recv() => {
                let message = match message {
                    Some(Ok(Message::Text(text))) => UpstreamMessage::Text(text.as_str().to_owned().into()),
                    Some(Ok(Message::Binary(bytes))) => UpstreamMessage::Binary(bytes),
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                    _ => break,
                };
                if !matches!(tokio::time::timeout(WRITE_TIMEOUT, upstream.send(message)).await, Ok(Ok(()))) { break; }
            },
            message = upstream.next() => {
                let message = match message {
                    Some(Ok(UpstreamMessage::Text(text))) => Message::Text(text.as_str().to_owned().into()),
                    Some(Ok(UpstreamMessage::Binary(bytes))) => Message::Binary(bytes),
                    Some(Ok(UpstreamMessage::Ping(_))) | Some(Ok(UpstreamMessage::Pong(_))) => continue,
                    _ => break,
                };
                if !matches!(tokio::time::timeout(WRITE_TIMEOUT, client.send(message)).await, Ok(Ok(()))) { break; }
            },
        }
    }
    // End the full-duplex session; never inject response.cancel (not supported by
    // this runtime), replay audio, retry a tool, or reconnect a conversation.
    let _ = tokio::time::timeout(Duration::from_secs(1), upstream.close(None)).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), client.close()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_origin_cannot_start_voice_on_another_site_or_peer() {
        let mut headers = HeaderMap::new();
        assert!(allowed_origin(&headers, "http://127.0.0.1:38475", true));
        for origin in ["null", "https://evil.example", "http://127.0.0.1:9999"] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(!allowed_origin(&headers, "http://127.0.0.1:38475", false));
        }
        headers.insert(header::ORIGIN, "http://127.0.0.1:38475".parse().unwrap());
        assert!(allowed_origin(&headers, "http://127.0.0.1:38475", false));
        assert!(!allowed_origin(&headers, "http://127.0.0.1:38475", true));
    }

    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (url, task)
    }

    async fn echo(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(message)) = socket.recv().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
                if socket.send(message).await.is_err() {
                    break;
                }
            }
        })
    }
    async fn test_bridge(
        State((url, cancel)): State<(String, tokio::sync::watch::Receiver<bool>)>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let upstream = connect(&url).await.unwrap();
        ws.on_upgrade(move |socket| bridge(socket, upstream, Some(cancel)))
    }

    #[tokio::test]
    async fn voice_two_hop_stream_preserves_audio_tool_results_and_cancellation() {
        let (origin, origin_task) = serve(Router::new().route("/", get(echo))).await;
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let (peer, peer_task) = serve(
            Router::new()
                .route("/", get(test_bridge))
                .with_state((origin, receiver)),
        )
        .await;
        let (_outer_cancel, outer_receiver) = tokio::sync::watch::channel(false);
        let (local, local_task) = serve(
            Router::new()
                .route("/", get(test_bridge))
                .with_state((peer, outer_receiver)),
        )
        .await;
        let mut client = connect(&local).await.unwrap();
        for payload in [
            r#"{"type":"session.update","session":{"tools":[{"name":"clock"}]}}"#,
            r#"{"type":"input_audio_buffer.append","audio":"AAAAAA=="}"#,
            r#"{"type":"response.function_call_arguments.done","call_id":"c1","name":"clock","arguments":"{}"}"#,
            r#"{"type":"conversation.item.create","item":{"type":"function_call_output","call_id":"c1","output":"12:00"}}"#,
            r#"{"type":"response.output_audio.delta","delta":"AAAAAA=="}"#,
        ] {
            client
                .send(UpstreamMessage::Text(payload.into()))
                .await
                .unwrap();
            let echoed = tokio::time::timeout(Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(echoed.into_text().unwrap(), payload);
        }
        cancel.send_replace(true);
        let closed = tokio::time::timeout(Duration::from_secs(3), client.next())
            .await
            .unwrap();
        assert!(matches!(closed, None | Some(Ok(UpstreamMessage::Close(_)))));
        origin_task.abort();
        peer_task.abort();
        local_task.abort();
    }

    #[tokio::test]
    async fn voice_upstream_failure_is_actionable() {
        let (url, task) =
            serve(Router::new().route("/", get(|| async { StatusCode::SERVICE_UNAVAILABLE })))
                .await;
        let err = connect(&url).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("Prepare"));
        task.abort();
    }

    #[tokio::test]
    async fn voice_lease_loads_health_route_blocks_unload_and_force_cancels() {
        use crate::{
            config::FleetConfig, fleet::FleetService, llama_swap::LlamaSwapSupervisor,
            telemetry::TelemetryStore,
        };
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let loaded = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route("/v1/models", get(|State(loaded): State<Arc<AtomicBool>>| async move {
                Json(serde_json::json!({"data":[{"id":"voice", "status":{"value":if loaded.load(Ordering::SeqCst) {"loaded"} else {"unloaded"}},
                    "meta":{"llamaswap":{"kind":"voice", "runtime":"mlx_voicechat"}}}]}))
            }))
            .route("/running", get(|State(loaded): State<Arc<AtomicBool>>| async move {
                Json(serde_json::json!({"running":if loaded.load(Ordering::SeqCst) { vec![serde_json::json!({"model":"voice","state":"ready","proxy":"http://127.0.0.1:9999"})] } else { vec![] }}))
            }))
            .route("/upstream/voice/v1/realtime/health", get(|State(loaded): State<Arc<AtomicBool>>| async move {
                loaded.store(true, Ordering::SeqCst); "ok"
            }))
            .route("/api/models/unload", post(|State(loaded): State<Arc<AtomicBool>>| async move {
                loaded.store(false, Ordering::SeqCst); "ok"
            })).with_state(loaded);
        let (endpoint, task) = serve(app).await;
        let config: FleetConfig = serde_json::from_value(serde_json::json!({
            "schema_version":1,"local_host_id":"test","poll_interval_seconds":5,"request_timeout_ms":1000,"peer_api_port":38473,
            "hosts":[{"id":"test","display_name":"Test","address":"127.0.0.1","hardware":"test"}],
            "llama_swap":{"listen_address":endpoint.trim_start_matches("http://"),"config_path":"llama-swap.yaml"}
        })).unwrap();
        let directory =
            std::env::temp_dir().join(format!("relay-voice-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
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
        let (url, lease) = supervisor.begin_voice_session("voice").await.unwrap();
        assert!(url.ends_with("/upstream/voice/v1/realtime"));
        assert_eq!(
            supervisor.unload_models(false).await.unwrap().state,
            ControlState::Conflict
        );
        assert_eq!(fleet.snapshot().hosts[0].active_requests, 1);
        assert!(supervisor.unload_models(true).await.is_ok());
        assert!(*lease.cancelled.borrow());
        drop(lease);
        assert_eq!(fleet.snapshot().hosts[0].active_requests, 0);
        // A fresh request is allowed after a force unload, with a new lease.
        let (_, next) = supervisor.begin_voice_session("voice").await.unwrap();
        assert!(!*next.cancelled.borrow());
        drop(next);
        task.abort();
        drop(supervisor);
        drop(fleet);
        let _ = std::fs::remove_dir_all(directory);
    }
}
