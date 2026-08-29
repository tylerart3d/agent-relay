mod channels;
mod config;
mod config_watch;
mod discovery;
mod domain;
mod fleet;
mod fleet_proxy;
mod gateway;
mod gateway_runtime;
mod harness_setup;
mod hermes;
mod hermes_bridge;
mod llama_swap;
mod local_harness;
mod metrics;
mod opencode;
mod opencode_desktop;
mod peer_api;
mod pi_runner;
mod telemetry;
mod terminal;
mod tray;

use std::{io, sync::Arc, time::Duration};

use channels::{
    ChannelAdapterRegistry, ChannelAdapterStatus, ChannelAddress, ChannelRoute, ChannelRouteStore,
    SharedChannelAdapterRegistry, SharedChannelRouteStore,
};
use domain::FleetSnapshot;
use fleet::{FleetService, SharedFleetService};
use gateway::{GatewayCoordinator, SharedGatewayCoordinator};
use gateway_runtime::{GatewaySupervisor, SharedGatewaySupervisor};
use hermes::{HermesIntegration, SharedHermesIntegration};
use hermes_bridge::{HermesBridge, SharedHermesBridge};
use llama_swap::{LlamaSwapSupervisor, SharedLlamaSwapSupervisor};
use local_harness::{LocalHarnessIntegrations, SharedLocalHarnessIntegrations};
use opencode::{OpenCodeIntegration, SharedOpenCodeIntegration};
use pi_runner::PiRunner;
use tauri::{Emitter, Manager};
use tauri_plugin_autostart::ManagerExt;
use telemetry::{SharedTelemetry, TelemetryStore, TelemetrySummary};

#[derive(Clone, serde::Serialize)]
struct AppSettings {
    theme: config::ThemePreference,
    harness_visibility: config::HarnessVisibility,
    run_on_startup: bool,
    hermes_context_window: u32,
    opencode_context_window: u32,
    channel_gateway: config::ChannelGatewayConfig,
    photon_credentials_configured: bool,
    inference_overrides: std::collections::BTreeMap<String, domain::InferenceOverrides>,
}

#[tauri::command]
fn get_fleet_snapshot(state: tauri::State<'_, SharedFleetService>) -> FleetSnapshot {
    state.snapshot()
}

#[tauri::command]
fn get_telemetry_summary(
    range_hours: Option<u32>,
    telemetry: tauri::State<'_, SharedTelemetry>,
) -> Result<TelemetrySummary, String> {
    telemetry.summary(range_hours.unwrap_or(24))
}

#[tauri::command]
fn get_channel_routes(state: tauri::State<'_, SharedChannelRouteStore>) -> Vec<ChannelRoute> {
    state.list()
}

#[tauri::command]
fn get_channel_adapters(
    state: tauri::State<'_, SharedChannelAdapterRegistry>,
) -> Vec<ChannelAdapterStatus> {
    state.list()
}

#[tauri::command]
async fn execute_channel_command(
    channel: String,
    account_id: String,
    conversation_id: String,
    conversation_label: Option<String>,
    text: String,
    fleet: tauri::State<'_, SharedFleetService>,
) -> Result<serde_json::Value, String> {
    let address = ChannelAddress {
        channel: channel.clone(),
        account_id: account_id.clone(),
        conversation_id: conversation_id.clone(),
    };
    address.validate()?;
    let endpoint = format!(
        "{}/api/v1/channels/command",
        fleet.snapshot().proxy_endpoint.trim_end_matches('/')
    );
    let response = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| format!("failed to create channel command client: {error}"))?
        .post(endpoint)
        .json(&serde_json::json!({
            "channel": channel,
            "account_id": account_id,
            "conversation_id": conversation_id,
            "conversation_label": conversation_label,
            "sender_id": "agent-relay-ui",
            "text": text,
        }))
        .send()
        .await
        .map_err(|error| format!("Agent Relay channel API is unavailable: {error}"))?;
    let status = response.status();
    let mut body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| format!("channel API returned invalid JSON: {error}"))?;
    if let Some(object) = body.as_object_mut() {
        object.insert("http_status".into(), serde_json::json!(status.as_u16()));
    }
    Ok(body)
}

#[tauri::command]
fn show_status_window(app: tauri::AppHandle) {
    tray::show_window(&app);
    let _ = app.emit("status-window-opened", ());
}

#[tauri::command]
fn get_app_settings(
    app: tauri::AppHandle,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<AppSettings, String> {
    read_app_settings(&app, &gateway)
}

fn read_app_settings(
    app: &tauri::AppHandle,
    gateway: &SharedGatewaySupervisor,
) -> Result<AppSettings, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    let ui = config::get_ui_config(&config_dir)?;
    let (hermes_context_window, opencode_context_window) =
        config::get_client_context_windows(&config_dir)?;
    let run_on_startup = app
        .autolaunch()
        .is_enabled()
        .map_err(|error| error.to_string())?;
    Ok(AppSettings {
        theme: ui.theme,
        harness_visibility: ui.harness_visibility,
        run_on_startup,
        hermes_context_window,
        opencode_context_window,
        channel_gateway: config::get_channel_gateway_config(&config_dir)?,
        photon_credentials_configured: gateway.credentials_configured(),
        inference_overrides: config::get_inference_overrides(&config_dir)?,
    })
}

#[tauri::command]
fn set_model_inference_override(
    app: tauri::AppHandle,
    qualified_model: String,
    inference_override: domain::InferenceOverrides,
    fleet: tauri::State<'_, SharedFleetService>,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<AppSettings, String> {
    let (host_id, model_id) = qualified_model
        .split_once('/')
        .filter(|(host, model)| !host.is_empty() && !model.is_empty())
        .ok_or_else(|| "model must use the form <host>/<profile>".to_owned())?;
    let snapshot = fleet.snapshot();
    let profile = snapshot
        .hosts
        .iter()
        .find(|host| host.id == host_id)
        .and_then(|host| host.models.iter().find(|model| model.id == model_id))
        .ok_or_else(|| format!("unknown model profile: {qualified_model}"))?;
    profile.validate_inference_override(&inference_override)?;
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    config::set_inference_override(&config_dir, qualified_model, inference_override)?;
    read_app_settings(&app, &gateway)
}

#[tauri::command]
async fn set_channel_gateway(
    app: tauri::AppHandle,
    request: GatewayPlacementRequest,
    coordinator: tauri::State<'_, SharedGatewayCoordinator>,
    supervisor: tauri::State<'_, SharedGatewaySupervisor>,
    fleet: tauri::State<'_, SharedFleetService>,
) -> Result<AppSettings, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    let mut gateway = coordinator.config();
    gateway.primary_host_id = request.primary_host_id;
    gateway.secondary_host_id = request.secondary_host_id;
    gateway.automatic_failover = request.automatic_failover;
    gateway.failover_after_seconds = request.failover_after_seconds;
    let gateway = config::set_channel_gateway_config(&config_dir, gateway)?;
    coordinator.update_config(gateway.clone())?;
    supervisor.restart();
    synchronize_gateway_config(&fleet, &gateway).await;
    read_app_settings(&app, &supervisor)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayPlacementRequest {
    primary_host_id: Option<String>,
    secondary_host_id: Option<String>,
    automatic_failover: bool,
    failover_after_seconds: u64,
}

#[tauri::command]
async fn configure_photon_gateway(
    app: tauri::AppHandle,
    project_id: String,
    project_secret: Option<String>,
    allowed_senders: Vec<String>,
    coordinator: tauri::State<'_, SharedGatewayCoordinator>,
    supervisor: tauri::State<'_, SharedGatewaySupervisor>,
    fleet: tauri::State<'_, SharedFleetService>,
) -> Result<AppSettings, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    let mut gateway = coordinator.config();
    gateway.photon_project_id = Some(project_id.trim().to_owned());
    gateway.allowed_senders = allowed_senders
        .into_iter()
        .map(|sender| sender.trim().to_owned())
        .filter(|sender| !sender.is_empty())
        .collect();
    let gateway = config::set_channel_gateway_config(&config_dir, gateway)?;
    let local_eligible = gateway.primary_host_id.as_deref() == Some(fleet.local_host_id())
        || gateway.secondary_host_id.as_deref() == Some(fleet.local_host_id());
    if local_eligible {
        if let Some(secret) = project_secret.as_deref() {
            supervisor.store_secret(secret)?;
        }
    }
    coordinator.update_config(gateway.clone())?;
    supervisor.restart();
    synchronize_gateway_config(&fleet, &gateway).await;
    if let Some(secret) = project_secret.as_deref() {
        provision_remote_gateway_credentials(&fleet, &gateway, secret).await;
    }
    read_app_settings(&app, &supervisor)
}

#[tauri::command]
fn clear_photon_gateway_credentials(
    app: tauri::AppHandle,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<AppSettings, String> {
    gateway.clear_secret()?;
    read_app_settings(&app, &gateway)
}

async fn provision_remote_gateway_credentials(
    fleet: &SharedFleetService,
    gateway: &config::ChannelGatewayConfig,
    project_secret: &str,
) {
    let snapshot = fleet.snapshot();
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(1_500))
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return,
    };
    for host in snapshot.hosts.iter().filter(|host| {
        host.id != snapshot.local_host_id
            && host.connection != domain::ConnectionState::Offline
            && (gateway.primary_host_id.as_deref() == Some(host.id.as_str())
                || gateway.secondary_host_id.as_deref() == Some(host.id.as_str()))
    }) {
        let endpoint = format!(
            "http://{}:{}/api/v1/channels/gateway/provision",
            host.address,
            fleet.peer_api_port()
        );
        if let Err(error) = client
            .post(endpoint)
            .json(&serde_json::json!({
                "config": gateway,
                "project_secret": project_secret,
            }))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            eprintln!(
                "failed to provision Photon credentials on {}: {error}",
                host.id
            );
        }
    }
}

async fn synchronize_gateway_config(
    fleet: &SharedFleetService,
    gateway: &config::ChannelGatewayConfig,
) {
    let snapshot = fleet.refresh().await;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(1_500))
        .timeout(Duration::from_secs(5))
        .build();
    let Ok(client) = client else {
        return;
    };
    for host in snapshot.hosts.iter().filter(|host| {
        host.id != snapshot.local_host_id && host.connection != domain::ConnectionState::Offline
    }) {
        let endpoint = format!(
            "http://{}:{}/api/v1/channels/gateway/config",
            host.address,
            fleet.peer_api_port()
        );
        if let Err(error) = client
            .post(endpoint)
            .json(&gateway)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            eprintln!(
                "failed to synchronize gateway placement with {}: {error}",
                host.id
            );
        }
    }
}

#[tauri::command]
fn set_harness_visible(
    app: tauri::AppHandle,
    harness: String,
    visible: bool,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<AppSettings, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    config::set_harness_visible(&config_dir, &harness, visible)?;
    read_app_settings(&app, &gateway)
}

#[tauri::command]
fn set_theme(
    app: tauri::AppHandle,
    theme: config::ThemePreference,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<AppSettings, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    let ui = config::set_theme(&config_dir, theme)?;
    app.emit("theme-changed", &ui.theme)
        .map_err(|error| error.to_string())?;
    read_app_settings(&app, &gateway)
}

#[tauri::command]
fn set_run_on_startup(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    if enabled {
        app.autolaunch().enable()
    } else {
        app.autolaunch().disable()
    }
    .map_err(|error| error.to_string())?;
    app.autolaunch()
        .is_enabled()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_client_context_window(
    app: tauri::AppHandle,
    client: String,
    context_window: u32,
    fleet: tauri::State<'_, SharedFleetService>,
    hermes: tauri::State<'_, SharedHermesIntegration>,
    opencode: tauri::State<'_, SharedOpenCodeIntegration>,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<AppSettings, String> {
    match client.as_str() {
        "hermes" => {
            let status = hermes.set_context_window(context_window)?;
            fleet.update_hermes_status(status);
        }
        "opencode" => {
            let status = opencode.set_context_window(context_window, &fleet.snapshot())?;
            fleet.update_opencode_status(status);
        }
        _ => return Err(format!("unknown client '{client}'")),
    }
    read_app_settings(&app, &gateway)
}

#[tauri::command]
async fn quit_app(
    app: tauri::AppHandle,
    force: bool,
    fleet: tauri::State<'_, SharedFleetService>,
    gateway: tauri::State<'_, SharedGatewaySupervisor>,
) -> Result<domain::ControlOutcome, String> {
    let drain = match fleet_proxy::begin_generation_drain() {
        Ok(drain) => drain,
        Err(active_requests) => {
            let snapshot = fleet.snapshot();
            return Ok(domain::ControlOutcome {
                state: domain::ControlState::Conflict,
                host_id: snapshot.local_host_id,
                active_requests,
                loaded_model_id: None,
                message: "Agent Relay shutdown is already in progress".to_owned(),
            });
        }
    };
    let active_proxy_requests = drain.active_requests();
    if active_proxy_requests > 0 && !force {
        let snapshot = fleet.snapshot();
        let loaded_model_id = snapshot
            .hosts
            .iter()
            .find(|host| host.id == snapshot.local_host_id)
            .and_then(|host| host.loaded_model_id.clone());
        return Ok(domain::ControlOutcome {
            state: domain::ControlState::Conflict,
            host_id: snapshot.local_host_id,
            active_requests: active_proxy_requests,
            loaded_model_id,
            message: format!(
                "{active_proxy_requests} request(s) are currently using this Agent Relay proxy"
            ),
        });
    }
    let outcome = service_preserving_quit_outcome(&fleet.snapshot(), active_proxy_requests);
    drain.commit();
    gateway.restart();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        app.exit(0);
    });
    Ok(outcome)
}

fn service_preserving_quit_outcome(
    snapshot: &FleetSnapshot,
    active_requests: u32,
) -> domain::ControlOutcome {
    let loaded_model_id = snapshot
        .hosts
        .iter()
        .find(|host| host.id == snapshot.local_host_id)
        .and_then(|host| host.loaded_model_id.clone());
    domain::ControlOutcome {
        state: domain::ControlState::Applied,
        host_id: snapshot.local_host_id.clone(),
        active_requests,
        loaded_model_id,
        message: "quit Agent Relay; local inference service remains running".to_owned(),
    }
}

#[tauri::command]
fn resize_tray_menu(app: tauri::AppHandle, height: f64) -> Result<(), String> {
    tray::resize_tray_menu(&app, height).map_err(|error| error.to_string())
}

#[tauri::command]
fn show_model_menu(
    app: tauri::AppHandle,
    host_id: String,
    anchor_y: f64,
    request_id: u64,
    menu_epoch: u64,
) -> Result<(), String> {
    tray::show_model_menu(&app, host_id, anchor_y, request_id, menu_epoch)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn resize_model_menu(app: tauri::AppHandle, height: f64, request_id: u64) -> Result<(), String> {
    tray::resize_model_menu(&app, height, request_id).map_err(|error| error.to_string())
}

#[tauri::command]
async fn hide_tray_menus(app: tauri::AppHandle) {
    tray::hide_menus(&app).await;
}

#[tauri::command]
fn set_opencode_enabled(
    enabled: bool,
    fleet: tauri::State<'_, SharedFleetService>,
    integration: tauri::State<'_, SharedOpenCodeIntegration>,
) -> Result<domain::OpenCodeStatus, String> {
    let status = integration.set_enabled(enabled, &fleet.snapshot())?;
    fleet.update_opencode_status(status.clone());
    Ok(status)
}

#[tauri::command]
fn set_hermes_enabled(
    enabled: bool,
    fleet: tauri::State<'_, SharedFleetService>,
    integration: tauri::State<'_, SharedHermesIntegration>,
) -> Result<domain::HermesStatus, String> {
    let snapshot = fleet.snapshot();
    let status = integration.set_enabled(enabled, &snapshot.proxy_endpoint)?;
    fleet.update_hermes_status(status.clone());
    fleet.update_hermes_cli_status(integration.cli_status());
    Ok(status)
}

#[tauri::command]
async fn refresh_fleet(
    state: tauri::State<'_, SharedFleetService>,
) -> Result<FleetSnapshot, String> {
    Ok(state.refresh().await)
}

#[tauri::command]
async fn get_harness_setup_statuses(
    host_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
) -> Result<Vec<harness_setup::HarnessSetupStatus>, String> {
    if fleet.is_local_host(&host_id) {
        return Ok(harness_setup::statuses(&fleet.snapshot()));
    }
    fleet.request_peer_harness_statuses(&host_id).await
}

#[tauri::command]
async fn get_opencode_sessions(
    host_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    opencode: tauri::State<'_, SharedOpenCodeIntegration>,
) -> Result<Vec<opencode::OpenCodeSessionInfo>, String> {
    if fleet.is_local_host(&host_id) {
        return opencode.list_sessions();
    }
    fleet.request_peer_opencode_sessions(&host_id).await
}

#[tauri::command]
async fn configure_fleet_harness(
    host_id: String,
    harness: harness_setup::HarnessId,
    fleet: tauri::State<'_, SharedFleetService>,
    hermes: tauri::State<'_, SharedHermesIntegration>,
    opencode: tauri::State<'_, SharedOpenCodeIntegration>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<harness_setup::HarnessSetupStatus, String> {
    let result = if fleet.is_local_host(&host_id) {
        harness_setup::configure(
            harness,
            fleet.inner(),
            hermes.inner(),
            opencode.inner(),
            harnesses.inner(),
        )
    } else {
        fleet
            .request_peer_harness_configure(
                &host_id,
                &harness_setup::HarnessSetupRequest { harness },
            )
            .await
    }?;
    fleet.refresh().await;
    Ok(result)
}

#[tauri::command]
async fn restart_local_llama_swap(
    force: bool,
    state: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::ControlOutcome, String> {
    state.inner().restart_service(force).await
}

#[tauri::command]
async fn stop_local_llama_swap(
    force: bool,
    state: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::ControlOutcome, String> {
    state.stop_service(force).await
}

#[tauri::command]
async fn load_model(
    host_id: String,
    model_id: String,
    force: bool,
    fleet: tauri::State<'_, SharedFleetService>,
    llama_swap: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::ControlOutcome, String> {
    let outcome = if fleet.is_local_host(&host_id) {
        llama_swap.load_model(&model_id, force).await?
    } else {
        let outcome = fleet
            .request_peer_load(
                &host_id,
                &domain::LoadModelRequest {
                    model_id: model_id.clone(),
                    force,
                    context_window: None,
                },
            )
            .await?;
        fleet.refresh().await;
        outcome
    };

    Ok(outcome)
}

async fn ensure_harness_context(
    host_id: &str,
    model_id: &str,
    context_window: u32,
    fleet: &SharedFleetService,
    llama_swap: &SharedLlamaSwapSupervisor,
) -> Result<FleetSnapshot, String> {
    config::validate_client_context_window(context_window)?;
    let snapshot = fleet.refresh().await;
    validate_running_model(&snapshot, host_id, model_id)?;
    let profile = snapshot
        .hosts
        .iter()
        .find(|host| host.id == host_id)
        .and_then(|host| host.models.iter().find(|profile| profile.id == model_id))
        .ok_or_else(|| format!("unknown profile: {host_id}/{model_id}"))?;
    if profile.context_length == Some(context_window) {
        return Ok(snapshot);
    }

    let outcome = if fleet.is_local_host(host_id) {
        llama_swap
            .load_model_with_context(model_id, false, Some(context_window))
            .await?
    } else {
        fleet
            .request_peer_load(
                host_id,
                &domain::LoadModelRequest {
                    model_id: model_id.to_owned(),
                    force: false,
                    context_window: Some(context_window),
                },
            )
            .await?
    };
    if outcome.state == domain::ControlState::Conflict {
        return Err(outcome.message);
    }
    Ok(fleet.refresh().await)
}

fn validate_running_model(
    snapshot: &FleetSnapshot,
    host_id: &str,
    model_id: &str,
) -> Result<(), String> {
    let host = snapshot
        .hosts
        .iter()
        .find(|host| host.id == host_id)
        .ok_or_else(|| format!("unknown fleet host: {host_id}"))?;
    if host.connection == domain::ConnectionState::Offline {
        return Err(format!("{} is offline", host.display_name));
    }
    if host.loaded_model_id.as_deref() != Some(model_id) {
        return Err(format!("{host_id}/{model_id} is not currently running"));
    }
    let profile = host
        .models
        .iter()
        .find(|profile| profile.id == model_id)
        .ok_or_else(|| format!("unknown profile: {host_id}/{model_id}"))?;
    if !profile.supports_text_inference() {
        return Err(format!(
            "{host_id}/{model_id} cannot be connected to a text client"
        ));
    }
    Ok(())
}

#[tauri::command]
async fn connect_hermes_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    hermes: tauri::State<'_, SharedHermesIntegration>,
    hermes_bridge: tauri::State<'_, SharedHermesBridge>,
    llama_swap: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::HermesStatus, String> {
    let snapshot = ensure_harness_context(
        &host_id,
        &model_id,
        hermes.context_window(),
        fleet.inner(),
        llama_swap.inner(),
    )
    .await?;
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "Hermes requires the OpenAI Chat Completions API",
    )?;
    let mut status = hermes.connect_model(&host_id, &model_id, &snapshot.proxy_endpoint)?;
    if status.state != domain::HermesSyncState::Synced {
        let error = status
            .error
            .clone()
            .unwrap_or_else(|| "Hermes integration is not enabled".to_owned());
        fleet.update_hermes_status(status);
        return Err(error);
    }

    fleet.update_hermes_status(status.clone());
    let revision = hermes_bridge.publish(fleet_proxy::ROUTED_MODEL_ID.to_owned());
    let bridge = hermes_bridge.inner().clone();
    let delivery = tauri::async_runtime::spawn_blocking(move || {
        bridge.wait_for_delivery(revision, Duration::from_secs(4))
    })
    .await
    .map_err(|error| format!("failed to wait for Hermes Desktop: {error}"))?;

    let delivery_error = match delivery {
        hermes_bridge::HermesDeliveryResult::Switched(_) => None,
        hermes_bridge::HermesDeliveryResult::Deferred(_) => Some(
            "Hermes Desktop deferred the switch and did not open the requested session".to_owned(),
        ),
        hermes_bridge::HermesDeliveryResult::Error(ack) => Some(
            ack.error
                .unwrap_or_else(|| "Hermes Desktop rejected the model switch".to_owned()),
        ),
        hermes_bridge::HermesDeliveryResult::TimedOut => Some(
            "Hermes Desktop did not acknowledge opening a new session; make sure it is running"
                .to_owned(),
        ),
        hermes_bridge::HermesDeliveryResult::Superseded => {
            Some("a newer Hermes model selection superseded this request".to_owned())
        }
    };
    if let Some(error) = delivery_error {
        status.state = domain::HermesSyncState::Error;
        status.last_synced_at_ms = None;
        status.error = Some(error.clone());
        fleet.update_hermes_status(status);
        return Err(error);
    }

    fleet.update_hermes_status(status.clone());
    Ok(status)
}

#[tauri::command]
async fn connect_opencode_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    opencode: tauri::State<'_, SharedOpenCodeIntegration>,
    llama_swap: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::OpenCodeStatus, String> {
    let snapshot = ensure_harness_context(
        &host_id,
        &model_id,
        opencode.context_window(),
        fleet.inner(),
        llama_swap.inner(),
    )
    .await?;
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "OpenCode requires the OpenAI Chat Completions API",
    )?;
    let status = opencode.connect_model(format!("{host_id}/{model_id}"), &snapshot)?;
    fleet.update_opencode_status(status.clone());
    if status.state != domain::OpenCodeSyncState::Synced {
        return Err(status
            .error
            .clone()
            .unwrap_or_else(|| "OpenCode integration is not enabled".to_owned()));
    }
    opencode_desktop::ensure_virtual_model().await?;
    Ok(status)
}

#[tauri::command]
async fn relaunch_opencode_desktop(selected_model: String) -> Result<(), String> {
    opencode_desktop::relaunch(&selected_model).await
}

#[tauri::command]
async fn connect_opencode_cli_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    opencode: tauri::State<'_, SharedOpenCodeIntegration>,
    llama_swap: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::OpenCodeStatus, String> {
    let snapshot = ensure_harness_context(
        &host_id,
        &model_id,
        opencode.context_window(),
        fleet.inner(),
        llama_swap.inner(),
    )
    .await?;
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "OpenCode requires the OpenAI Chat Completions API",
    )?;
    let status = opencode.connect_model(format!("{host_id}/{model_id}"), &snapshot)?;
    fleet.update_opencode_status(status.clone());
    if status.state != domain::OpenCodeSyncState::Synced {
        return Err(status
            .error
            .clone()
            .unwrap_or_else(|| "OpenCode integration is not enabled".to_owned()));
    }
    terminal::launch(terminal::CliHarness::OpenCode)?;
    Ok(status)
}

#[tauri::command]
async fn connect_hermes_cli_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    hermes: tauri::State<'_, SharedHermesIntegration>,
    llama_swap: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::HermesStatus, String> {
    let snapshot = ensure_harness_context(
        &host_id,
        &model_id,
        hermes.context_window(),
        fleet.inner(),
        llama_swap.inner(),
    )
    .await?;
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "Hermes requires the OpenAI Chat Completions API",
    )?;
    let status = hermes.connect_cli_model(&host_id, &model_id, &snapshot.proxy_endpoint)?;
    fleet.update_hermes_cli_status(status.clone());
    if status.state != domain::HermesSyncState::Synced {
        return Err(status
            .error
            .clone()
            .unwrap_or_else(|| "Hermes integration is not enabled".to_owned()));
    }
    terminal::launch_resolved(terminal::CliHarness::Hermes, &hermes.executable_path())?;
    Ok(status)
}

#[tauri::command]
fn connect_codex_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<domain::HarnessStatus, String> {
    let snapshot = fleet.snapshot();
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Responses,
        "Codex requires the OpenAI Responses API",
    )?;
    let status =
        harnesses.connect_codex(format!("{host_id}/{model_id}"), &snapshot.proxy_endpoint)?;
    fleet.update_codex_status(status.clone());
    terminal::launch(terminal::CliHarness::Codex)?;
    Ok(status)
}

#[tauri::command]
fn connect_claude_code_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<domain::HarnessStatus, String> {
    let snapshot = fleet.snapshot();
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::AnthropicMessages,
        "Claude Code requires the Anthropic Messages API",
    )?;
    let status =
        harnesses.connect_claude_code(format!("{host_id}/{model_id}"), &snapshot.proxy_endpoint)?;
    fleet.update_claude_code_status(status.clone());
    terminal::launch(terminal::CliHarness::ClaudeCode)?;
    Ok(status)
}

#[tauri::command]
fn connect_pi_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<domain::HarnessStatus, String> {
    let snapshot = fleet.snapshot();
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "Pi requires the OpenAI Chat Completions API",
    )?;
    let selected_model = format!("{host_id}/{model_id}");
    let context_window = local_harness::model_context_window(&snapshot, &selected_model);
    let status = harnesses.connect_pi(selected_model, &snapshot.proxy_endpoint, context_window)?;
    fleet.update_pi_status(status.clone());
    terminal::launch(terminal::CliHarness::Pi)?;
    Ok(status)
}

#[tauri::command]
fn connect_copilot_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<domain::HarnessStatus, String> {
    let snapshot = fleet.snapshot();
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "Copilot CLI requires OpenAI Chat Completions, streaming, and tool calling",
    )?;
    let selected_model = format!("{host_id}/{model_id}");
    let status = harnesses.connect_copilot(selected_model.clone(), &snapshot.proxy_endpoint)?;
    fleet.update_copilot_status(status.clone());
    terminal::launch_with_env(
        terminal::CliHarness::Copilot,
        &copilot_terminal_environment(&selected_model, &snapshot.proxy_endpoint),
    )?;
    Ok(status)
}

#[tauri::command]
fn connect_vscode_model(
    host_id: String,
    model_id: String,
    fleet: tauri::State<'_, SharedFleetService>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<domain::HarnessStatus, String> {
    let snapshot = fleet.snapshot();
    validate_running_model_capability(
        &snapshot,
        &host_id,
        &model_id,
        domain::ProfileCapability::Chat,
        "VS Code agents require OpenAI Chat Completions and tool calling",
    )?;
    let status =
        harnesses.connect_vscode(format!("{host_id}/{model_id}"), &snapshot.proxy_endpoint)?;
    fleet.update_vscode_status(status.clone());
    Ok(status)
}

fn validate_running_model_capability(
    snapshot: &FleetSnapshot,
    host_id: &str,
    model_id: &str,
    capability: domain::ProfileCapability,
    requirement: &str,
) -> Result<(), String> {
    validate_running_model(snapshot, host_id, model_id)?;
    let profile = snapshot
        .hosts
        .iter()
        .find(|host| host.id == host_id)
        .and_then(|host| host.models.iter().find(|profile| profile.id == model_id))
        .expect("running model was already validated");
    if !profile.capabilities.contains(&capability) {
        return Err(format!(
            "{host_id}/{model_id} is incompatible: {requirement}"
        ));
    }
    Ok(())
}

#[tauri::command]
fn launch_cli(
    client: String,
    fleet: tauri::State<'_, SharedFleetService>,
    hermes: tauri::State<'_, SharedHermesIntegration>,
    opencode: tauri::State<'_, SharedOpenCodeIntegration>,
    harnesses: tauri::State<'_, SharedLocalHarnessIntegrations>,
) -> Result<(), String> {
    let harness = terminal::CliHarness::parse(&client)?;
    let snapshot = fleet.snapshot();
    let selected_model = selected_cli_model(&snapshot, harness)
        .ok_or_else(|| format!("choose a model for {client} before launching it"))?;
    let (host_id, model_id) = selected_model
        .split_once('/')
        .ok_or_else(|| format!("invalid selected model: {selected_model}"))?;
    let (capability, requirement) = match harness {
        terminal::CliHarness::OpenCode => (
            domain::ProfileCapability::Chat,
            "OpenCode requires the OpenAI Chat Completions API",
        ),
        terminal::CliHarness::Hermes => (
            domain::ProfileCapability::Chat,
            "Hermes requires the OpenAI Chat Completions API",
        ),
        terminal::CliHarness::Codex => (
            domain::ProfileCapability::Responses,
            "Codex requires the OpenAI Responses API",
        ),
        terminal::CliHarness::ClaudeCode => (
            domain::ProfileCapability::AnthropicMessages,
            "Claude Code requires the Anthropic Messages API",
        ),
        terminal::CliHarness::Pi => (
            domain::ProfileCapability::Chat,
            "Pi requires the OpenAI Chat Completions API",
        ),
        terminal::CliHarness::Copilot => (
            domain::ProfileCapability::Chat,
            "Copilot CLI requires the OpenAI Chat Completions API",
        ),
    };
    validate_running_model_capability(&snapshot, host_id, model_id, capability, requirement)?;

    match harness {
        terminal::CliHarness::OpenCode => {
            let status = opencode.connect_model(selected_model.to_owned(), &snapshot)?;
            fleet.update_opencode_status(status.clone());
            if status.state != domain::OpenCodeSyncState::Synced {
                return Err(status
                    .error
                    .unwrap_or_else(|| "OpenCode integration is not enabled".to_owned()));
            }
        }
        terminal::CliHarness::Hermes => {
            let status = hermes.connect_cli_model(host_id, model_id, &snapshot.proxy_endpoint)?;
            fleet.update_hermes_cli_status(status.clone());
            if status.state != domain::HermesSyncState::Synced {
                return Err(status
                    .error
                    .unwrap_or_else(|| "Hermes integration is not enabled".to_owned()));
            }
        }
        terminal::CliHarness::Codex => {
            let status =
                harnesses.connect_codex(selected_model.to_owned(), &snapshot.proxy_endpoint)?;
            fleet.update_codex_status(status);
        }
        terminal::CliHarness::ClaudeCode => {
            let status = harnesses
                .connect_claude_code(selected_model.to_owned(), &snapshot.proxy_endpoint)?;
            fleet.update_claude_code_status(status);
        }
        terminal::CliHarness::Pi => {
            let context_window = local_harness::model_context_window(&snapshot, selected_model);
            let status = harnesses.connect_pi(
                selected_model.to_owned(),
                &snapshot.proxy_endpoint,
                context_window,
            )?;
            fleet.update_pi_status(status);
        }
        terminal::CliHarness::Copilot => {
            let status =
                harnesses.connect_copilot(selected_model.to_owned(), &snapshot.proxy_endpoint)?;
            fleet.update_copilot_status(status);
        }
    }

    if matches!(harness, terminal::CliHarness::Copilot) {
        terminal::launch_with_env(
            harness,
            &copilot_terminal_environment(selected_model, &snapshot.proxy_endpoint),
        )
    } else if matches!(harness, terminal::CliHarness::Hermes) {
        terminal::launch_resolved(harness, &hermes.executable_path())
    } else {
        terminal::launch(harness)
    }
}

fn selected_cli_model(snapshot: &FleetSnapshot, harness: terminal::CliHarness) -> Option<&str> {
    match harness {
        terminal::CliHarness::OpenCode => snapshot.opencode.selected_model.as_deref(),
        terminal::CliHarness::Hermes => snapshot.hermes_cli.selected_model.as_deref(),
        terminal::CliHarness::Codex => snapshot.codex.selected_model.as_deref(),
        terminal::CliHarness::ClaudeCode => snapshot.claude_code.selected_model.as_deref(),
        terminal::CliHarness::Pi => snapshot.pi.selected_model.as_deref(),
        terminal::CliHarness::Copilot => snapshot.copilot.selected_model.as_deref(),
    }
}

fn copilot_terminal_environment(
    selected_model: &str,
    proxy_endpoint: &str,
) -> Vec<(String, String)> {
    vec![
        (
            "COPILOT_PROVIDER_BASE_URL".to_owned(),
            format!("{}/v1", proxy_endpoint.trim_end_matches('/')),
        ),
        ("COPILOT_PROVIDER_TYPE".to_owned(), "openai".to_owned()),
        (
            "COPILOT_PROVIDER_API_KEY".to_owned(),
            "agentrelay-local".to_owned(),
        ),
        ("COPILOT_MODEL".to_owned(), selected_model.to_owned()),
    ]
}

#[tauri::command]
async fn unload_host(
    host_id: String,
    force: bool,
    fleet: tauri::State<'_, SharedFleetService>,
    llama_swap: tauri::State<'_, SharedLlamaSwapSupervisor>,
) -> Result<domain::ControlOutcome, String> {
    if fleet.is_local_host(&host_id) {
        return llama_swap.unload_models(force).await;
    }

    let outcome = fleet
        .request_peer_unload(&host_id, &domain::UnloadModelsRequest { force })
        .await?;
    fleet.refresh().await;
    Ok(outcome)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Ok(cursor) = app.cursor_position() {
                tray::show_tray_menu(app, cursor);
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let machine_name = hostname::get()
                .map_err(|error| io::Error::other(format!("failed to read hostname: {error}")))?
                .to_string_lossy()
                .into_owned();
            let config_dir = app.path().app_config_dir()?;
            let config = config::FleetConfig::load_or_create(&config_dir, &machine_name)
                .map_err(io::Error::other)?;
            let llama_swap_config = config.llama_swap.clone();
            let opencode_config = config.opencode.clone();
            let hermes_config = config.hermes.clone();
            let codex_config = config.codex.clone();
            let claude_code_config = config.claude_code.clone();
            let pi_config = config.pi.clone();
            let copilot_config = config.copilot.clone();
            let vscode_config = config.vscode.clone();
            let gateway_config = config.channel_gateway.clone();
            let local_host_id = config.local_host_id.clone();
            let telemetry = Arc::new(TelemetryStore::new(&config_dir).map_err(io::Error::other)?);
            let service =
                Arc::new(FleetService::new(config, config_dir.clone()).map_err(io::Error::other)?);
            let llama_swap = Arc::new(
                LlamaSwapSupervisor::new(
                    &llama_swap_config,
                    &config_dir,
                    service.clone(),
                    telemetry.clone(),
                )
                .map_err(io::Error::other)?,
            );
            let opencode = Arc::new(OpenCodeIntegration::new(
                opencode_config,
                config_dir.clone(),
            ));
            let hermes = Arc::new(HermesIntegration::new(hermes_config, config_dir.clone()));
            let hermes_bridge = Arc::new(HermesBridge::default());
            let channel_routes =
                Arc::new(ChannelRouteStore::new(&config_dir).map_err(io::Error::other)?);
            let channel_adapters = Arc::new(ChannelAdapterRegistry::default());
            let gateway = Arc::new(GatewayCoordinator::new(local_host_id, gateway_config));
            let gateway_supervisor = Arc::new(GatewaySupervisor::new(
                config_dir.clone(),
                gateway.clone(),
                service.clone(),
            ));
            let harnesses = Arc::new(LocalHarnessIntegrations::new(
                codex_config,
                claude_code_config,
                pi_config,
                copilot_config,
                vscode_config,
                config_dir.clone(),
            ));
            let pi_runner = Arc::new(PiRunner::new(harnesses.clone()));
            service.update_hermes_status(hermes.prepare(&service.snapshot().proxy_endpoint));
            service.update_hermes_cli_status(hermes.cli_status());
            service.update_codex_status(harnesses.codex_status());
            service.update_claude_code_status(harnesses.claude_code_status());
            service.update_pi_status(harnesses.pi_status());
            service.update_copilot_status(harnesses.copilot_status());
            service.update_vscode_status(harnesses.vscode_status());

            app.manage(service.clone());
            app.manage(llama_swap.clone());
            app.manage(opencode.clone());
            app.manage(hermes.clone());
            app.manage(hermes_bridge.clone());
            app.manage(channel_routes.clone());
            app.manage(channel_adapters.clone());
            app.manage(gateway.clone());
            app.manage(gateway_supervisor.clone());
            app.manage(harnesses.clone());
            app.manage(pi_runner.clone());
            app.manage(telemetry.clone());
            tray::setup(app)?;
            llama_swap.start().map_err(io::Error::other)?;
            gateway_supervisor.clone().start();
            let config_watch_app = app.handle().clone();
            let watched_config = llama_swap.config_path().to_owned();
            tauri::async_runtime::spawn(config_watch::watch(config_watch_app, watched_config));
            let peer_service = service.clone();
            tauri::async_runtime::spawn(peer_api::supervise(
                peer_service,
                llama_swap.clone(),
                peer_api::PeerIntegrations {
                    hermes: hermes.clone(),
                    opencode: opencode.clone(),
                    pi: pi_runner.clone(),
                    harnesses: harnesses.clone(),
                },
                telemetry.clone(),
                gateway.clone(),
                gateway_supervisor.clone(),
            ));
            let proxy_service = service.clone();
            let proxy_llama_swap = llama_swap.clone();
            let proxy_opencode = opencode.clone();
            let proxy_telemetry = telemetry.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = fleet_proxy::serve(
                    proxy_service,
                    fleet_proxy::ProxyIntegrations {
                        hermes_bridge,
                        hermes,
                        opencode: proxy_opencode,
                        pi: pi_runner,
                    },
                    proxy_llama_swap,
                    channel_routes,
                    channel_adapters,
                    gateway,
                    proxy_telemetry,
                )
                .await
                {
                    eprintln!("{error}");
                }
            });
            let metrics_service = service.clone();
            tauri::async_runtime::spawn(metrics::monitor(metrics_service, telemetry));
            let poll_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut desktop_refresh_attempted = false;
                loop {
                    let snapshot = service.refresh().await;
                    let status = opencode.sync(&snapshot);
                    let should_refresh = !desktop_refresh_attempted
                        && status.state == domain::OpenCodeSyncState::Synced;
                    service.update_opencode_status(status);
                    if should_refresh {
                        match opencode_desktop::refresh_running_desktop().await {
                            Ok(completed) => desktop_refresh_attempted = completed,
                            Err(error) => {
                                eprintln!("failed to refresh OpenCode Desktop: {error}");
                                desktop_refresh_attempted = true;
                            }
                        }
                    }
                    let interval =
                        service.adaptive_poll_interval(tray::control_surface_visible(&poll_app));
                    tokio::time::sleep(interval).await;
                }
            });
            if std::env::args_os().any(|argument| argument == "--show-menu") {
                let menu_app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Ok(cursor) = menu_app.cursor_position() {
                        tray::show_tray_menu(&menu_app, cursor);
                    }
                });
            } else if std::env::args_os().any(|argument| argument == "--show") {
                let menu_app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Ok(cursor) = menu_app.cursor_position() {
                        tray::show_tray_menu(&menu_app, cursor);
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if window.label() == "main" {
                    let _ = window.app_handle().emit("status-window-closing", ());
                }
                let _ = window.hide();
            }
            if matches!(window.label(), "tray-menu" | "model-menu")
                && matches!(event, tauri::WindowEvent::Focused(false))
            {
                let menu_app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(70)).await;
                    if !tray::menus_have_focus(&menu_app) {
                        tray::hide_menus(&menu_app).await;
                    }
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_fleet_snapshot,
            get_telemetry_summary,
            get_channel_routes,
            get_channel_adapters,
            execute_channel_command,
            show_status_window,
            get_app_settings,
            set_theme,
            set_harness_visible,
            set_run_on_startup,
            set_client_context_window,
            set_model_inference_override,
            set_channel_gateway,
            configure_photon_gateway,
            clear_photon_gateway_credentials,
            quit_app,
            resize_tray_menu,
            show_model_menu,
            resize_model_menu,
            hide_tray_menus,
            set_opencode_enabled,
            set_hermes_enabled,
            refresh_fleet,
            get_harness_setup_statuses,
            get_opencode_sessions,
            configure_fleet_harness,
            restart_local_llama_swap,
            stop_local_llama_swap,
            load_model,
            connect_hermes_model,
            connect_hermes_cli_model,
            connect_opencode_model,
            relaunch_opencode_desktop,
            connect_opencode_cli_model,
            connect_codex_model,
            connect_claude_code_model,
            connect_pi_model,
            connect_copilot_model,
            connect_vscode_model,
            launch_cli,
            unload_host
        ])
        .build(tauri::generate_context!())
        .expect("error while building Agent Relay");

    app.run(|_, _| {});
}

#[cfg(test)]
mod orchestration_tests {
    use super::*;
    use crate::domain::{
        ConnectionState, HarnessStatus, HermesStatus, HostStatus, LlamaSwapStatus, ModelProfile,
        OpenCodeStatus, PeerApiStatus, ProfileCapability,
    };

    fn snapshot() -> FleetSnapshot {
        FleetSnapshot {
            local_host_id: "workstation".into(),
            config_path: "fleet.json".into(),
            proxy_endpoint: "http://127.0.0.1:38475".into(),
            refreshed_at_ms: 1,
            peer_api: PeerApiStatus::default(),
            hosts: vec![HostStatus {
                id: "workstation".into(),
                display_name: "WORKSTATION".into(),
                address: "workstation".into(),
                hardware: String::new(),
                connection: ConnectionState::Local,
                models: vec![ModelProfile {
                    id: "ornith".into(),
                    display_name: "Ornith".into(),
                    runtime: "llama.cpp".into(),
                    kind: Default::default(),
                    capabilities: vec![ProfileCapability::Chat],
                    lifecycle_adapter: "llama_swap".into(),
                    resource_pool: "default".into(),
                    context_length: None,
                    inference_controls: Default::default(),
                }],
                loaded_model_id: Some("ornith".into()),
                active_requests: 0,
                memory_used_bytes: None,
                memory_total_bytes: None,
                memory_kind: None,
                tokens_per_second: None,
                aggregate_tokens_per_second: None,
                throughput_concurrency: 0,
                last_seen_at_ms: None,
                error: None,
                llama_swap: LlamaSwapStatus::default(),
                channel_gateway: None,
            }],
            opencode: OpenCodeStatus {
                selected_model: Some("workstation/opencode".into()),
                ..OpenCodeStatus::default()
            },
            hermes: HermesStatus {
                selected_model: Some("workstation/desktop".into()),
                ..HermesStatus::default()
            },
            hermes_cli: HermesStatus {
                selected_model: Some("workstation/cli".into()),
                ..HermesStatus::default()
            },
            codex: HarnessStatus::default(),
            claude_code: HarnessStatus::default(),
            pi: HarnessStatus::default(),
            copilot: HarnessStatus::default(),
            vscode: HarnessStatus::default(),
        }
    }

    #[test]
    fn cli_selection_keeps_hermes_desktop_and_cli_independent() {
        let snapshot = snapshot();
        assert_eq!(
            selected_cli_model(&snapshot, terminal::CliHarness::Hermes),
            Some("workstation/cli")
        );
        assert_eq!(
            snapshot.hermes.selected_model.as_deref(),
            Some("workstation/desktop")
        );
    }

    #[test]
    fn connector_validation_requires_the_loaded_model_and_capability() {
        let mut snapshot = snapshot();
        assert!(validate_running_model_capability(
            &snapshot,
            "workstation",
            "ornith",
            ProfileCapability::Chat,
            "chat required",
        )
        .is_ok());
        assert!(validate_running_model_capability(
            &snapshot,
            "workstation",
            "ornith",
            ProfileCapability::Responses,
            "responses required",
        )
        .is_err());

        snapshot.hosts[0].loaded_model_id = None;
        assert!(validate_running_model(&snapshot, "workstation", "ornith").is_err());
    }

    #[test]
    fn quitting_preserves_the_loaded_service_in_the_outcome() {
        let outcome = service_preserving_quit_outcome(&snapshot(), 0);
        assert_eq!(outcome.state, domain::ControlState::Applied);
        assert_eq!(outcome.loaded_model_id.as_deref(), Some("ornith"));
        assert!(outcome.message.contains("service remains running"));
    }
}
