//! Explicit loopback CLI operations. No arbitrary Tauri invocation or availability overrides.
use crate::*;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::Manager;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Request {
    Settings,
    Harnesses {
        host: String,
    },
    Configure {
        host: String,
        harness: harness_setup::HarnessId,
    },
    Route {
        client: String,
        model: String,
    },
    Disconnect {
        client: String,
    },
    Reload {
        client: String,
        model: String,
        #[serde(default)]
        force: bool,
    },
    Inference {
        client: String,
        #[serde(default)]
        effort: Option<domain::ReasoningEffort>,
        #[serde(default)]
        reset: bool,
    },
    Launch {
        client: String,
        directory: String,
    },
    Service {
        operation: ServiceOperation,
        #[serde(default)]
        force: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ServiceOperation {
    Start,
    Stop,
    Restart,
}

fn value<T: serde::Serialize>(result: Result<T, String>) -> Result<Value, String> {
    serde_json::to_value(result?).map_err(|e| e.to_string())
}

pub(crate) async fn execute(app: tauri::AppHandle, request: Request) -> Result<Value, String> {
    let fleet = app.state::<SharedFleetService>();
    let hermes = app.state::<SharedHermesIntegration>();
    let opencode = app.state::<SharedOpenCodeIntegration>();
    let harnesses = app.state::<SharedLocalHarnessIntegrations>();
    let llama = app.state::<SharedLlamaSwapSupervisor>();
    match request {
        Request::Settings => value(read_app_settings(&app)),
        Request::Harnesses { host } => value(get_harness_setup_statuses(host, fleet).await),
        Request::Configure { host, harness } => {
            value(configure_fleet_harness(host, harness, fleet, hermes, opencode, harnesses).await)
        }
        Request::Disconnect { client } => {
            value(disconnect_client_route(client, fleet, hermes, opencode))
        }
        Request::Reload {
            client,
            model,
            force,
        } => value(reload_client_model(client, model, force, fleet, llama).await),
        Request::Inference {
            client,
            effort,
            reset,
        } => {
            if reset == effort.is_some() {
                return Err("inference takes either --thinking LEVEL or --reset".into());
            }
            value(config::set_client_reasoning_effort(
                &app.path().app_config_dir().map_err(|e| e.to_string())?,
                &client,
                effort,
            ))
        }
        Request::Launch { client, directory } => value(launch_cli(
            client,
            Some(directory),
            fleet,
            hermes,
            opencode,
            harnesses,
        )),
        Request::Service { operation, force } => match operation {
            ServiceOperation::Start => value(llama.start()),
            ServiceOperation::Stop => value(llama.stop_service(force).await),
            ServiceOperation::Restart => value(llama.restart_service(force).await),
        },
        Request::Route { client, model } => {
            // Routing never launches a terminal or restarts a desktop client.
            let (host, profile) = model
                .split_once('/')
                .filter(|(h, m)| !h.is_empty() && !m.is_empty())
                .ok_or("model must be <host>/<profile>")?;
            let snapshot = fleet.refresh().await;
            let capability = match client.as_str() {
                "codex" => domain::ProfileCapability::Responses,
                "claude_code" => domain::ProfileCapability::AnthropicMessages,
                "hermes" | "hermes_cli" | "opencode" | "opencode_cli" | "pi" | "copilot"
                | "vscode" | "openbot" => domain::ProfileCapability::Chat,
                _ => return Err(format!("unknown client: {client}")),
            };
            validate_running_model_capability(
                &snapshot,
                host,
                profile,
                capability,
                "model does not support this harness API",
            )?;
            match client.as_str() {
                "openbot" => {
                    fleet.set_openbot_route(Some(model.clone()))?;
                }
                "hermes" => {
                    let status = hermes.connect_model(host, profile, &snapshot.proxy_endpoint)?;
                    if status.state != domain::HermesSyncState::Synced {
                        return Err(status.error.unwrap_or("Hermes is not configured".into()));
                    }
                    fleet.update_hermes_status(status);
                }
                "hermes_cli" => {
                    let status =
                        hermes.connect_cli_model(host, profile, &snapshot.proxy_endpoint)?;
                    if status.state != domain::HermesSyncState::Synced {
                        return Err(status
                            .error
                            .unwrap_or("Hermes CLI is not configured".into()));
                    }
                    fleet.update_hermes_cli_status(status);
                }
                "opencode" | "opencode_cli" => {
                    let status = opencode.connect_model(model.clone(), &snapshot)?;
                    if status.state != domain::OpenCodeSyncState::Synced {
                        return Err(status.error.unwrap_or("OpenCode is not configured".into()));
                    }
                    fleet.update_opencode_status(status);
                }
                "codex" => fleet.update_codex_status(
                    harnesses.connect_codex(model.clone(), &snapshot.proxy_endpoint)?,
                ),
                "claude_code" => fleet.update_claude_code_status(
                    harnesses.connect_claude_code(model.clone(), &snapshot.proxy_endpoint)?,
                ),
                "pi" => fleet.update_pi_status(harnesses.connect_pi(
                    model.clone(),
                    &snapshot.proxy_endpoint,
                    local_harness::model_context_window(&snapshot, &model),
                )?),
                "copilot" => fleet.update_copilot_status(
                    harnesses.connect_copilot(model.clone(), &snapshot.proxy_endpoint)?,
                ),
                "vscode" => fleet.update_vscode_status(
                    harnesses.connect_vscode(model.clone(), &snapshot.proxy_endpoint)?,
                ),
                _ => unreachable!(),
            }
            Ok(json!({"client": client, "model": model}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_availability_and_arbitrary_actions() {
        for action in ["availability", "quit_app", "shell"] {
            assert!(serde_json::from_value::<Request>(json!({"action":action})).is_err());
        }
    }
    #[test]
    fn reload_requires_expected_model_and_defaults_to_safe() {
        assert!(
            serde_json::from_value::<Request>(json!({"action":"reload","client":"hermes"}))
                .is_err()
        );
        let request: Request = serde_json::from_value(
            json!({"action":"reload","client":"hermes","model":"mini/test"}),
        )
        .unwrap();
        assert!(matches!(request, Request::Reload { force: false, .. }));
    }
    #[test]
    fn service_rejects_unknown_operations_and_fields() {
        assert!(serde_json::from_value::<Request>(
            json!({"action":"service","operation":"delete"})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(
            json!({"action":"service","operation":"stop","host":"remote"})
        )
        .is_err());
    }
}
