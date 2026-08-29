use serde::{Deserialize, Serialize};

use crate::{
    domain::{
        ConnectionState, FleetSnapshot, HarnessSyncState, OpenCodeSyncState, ProfileCapability,
    },
    fleet::SharedFleetService,
    hermes::SharedHermesIntegration,
    local_harness::{model_context_window, SharedLocalHarnessIntegrations},
    opencode::SharedOpenCodeIntegration,
    terminal::{self, CliHarness},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessId {
    Hermes,
    HermesCli,
    OpenCode,
    OpenCodeCli,
    Codex,
    ClaudeCode,
    Pi,
    Copilot,
    Vscode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSetupState {
    NotInstalled,
    Detected,
    Configured,
    NeedsRepair,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessSetupStatus {
    pub id: HarnessId,
    pub label: String,
    pub state: HarnessSetupState,
    pub config_path: Option<String>,
    pub selected_model: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessSetupRequest {
    pub harness: HarnessId,
}

impl HarnessId {
    pub const ALL: [Self; 9] = [
        Self::Hermes,
        Self::HermesCli,
        Self::OpenCode,
        Self::OpenCodeCli,
        Self::Codex,
        Self::ClaudeCode,
        Self::Pi,
        Self::Copilot,
        Self::Vscode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Hermes => "Hermes",
            Self::HermesCli => "Hermes CLI",
            Self::OpenCode => "OpenCode",
            Self::OpenCodeCli => "OpenCode CLI",
            Self::Codex => "Codex CLI",
            Self::ClaudeCode => "Claude Code",
            Self::Pi => "Pi",
            Self::Copilot => "Copilot CLI",
            Self::Vscode => "VS Code",
        }
    }

    fn capability(self) -> ProfileCapability {
        match self {
            Self::Codex => ProfileCapability::Responses,
            Self::ClaudeCode => ProfileCapability::AnthropicMessages,
            _ => ProfileCapability::Chat,
        }
    }
}

pub fn statuses(snapshot: &FleetSnapshot) -> Vec<HarnessSetupStatus> {
    HarnessId::ALL
        .into_iter()
        .map(|harness| status(snapshot, harness))
        .collect()
}

pub fn configure(
    harness: HarnessId,
    fleet: &SharedFleetService,
    hermes: &SharedHermesIntegration,
    opencode: &SharedOpenCodeIntegration,
    harnesses: &SharedLocalHarnessIntegrations,
) -> Result<HarnessSetupStatus, String> {
    if !is_installed(harness) {
        return Err(format!(
            "{} is not installed on this machine",
            harness.label()
        ));
    }
    let snapshot = fleet.snapshot();
    let selected = preferred_model(&snapshot, harness)?;
    let (host_id, model_id) = selected
        .split_once('/')
        .ok_or_else(|| format!("invalid fleet model: {selected}"))?;

    match harness {
        HarnessId::Hermes => {
            let enabled = hermes.set_enabled(true, &snapshot.proxy_endpoint)?;
            fleet.update_hermes_status(enabled);
            let configured = hermes.connect_model(host_id, model_id, &snapshot.proxy_endpoint)?;
            fleet.update_hermes_status(configured);
        }
        HarnessId::HermesCli => {
            let enabled = hermes.set_enabled(true, &snapshot.proxy_endpoint)?;
            fleet.update_hermes_status(enabled);
            let configured =
                hermes.connect_cli_model(host_id, model_id, &snapshot.proxy_endpoint)?;
            fleet.update_hermes_cli_status(configured);
        }
        HarnessId::OpenCode | HarnessId::OpenCodeCli => {
            let enabled = opencode.set_enabled(true, &snapshot)?;
            fleet.update_opencode_status(enabled);
            let configured = opencode.connect_model(selected, &snapshot)?;
            fleet.update_opencode_status(configured);
        }
        HarnessId::Codex => {
            let configured = harnesses.connect_codex(selected, &snapshot.proxy_endpoint)?;
            fleet.update_codex_status(configured);
        }
        HarnessId::ClaudeCode => {
            let configured = harnesses.connect_claude_code(selected, &snapshot.proxy_endpoint)?;
            fleet.update_claude_code_status(configured);
        }
        HarnessId::Pi => {
            let context_window = model_context_window(&snapshot, &selected);
            let configured =
                harnesses.connect_pi(selected, &snapshot.proxy_endpoint, context_window)?;
            fleet.update_pi_status(configured);
        }
        HarnessId::Copilot => {
            let configured = harnesses.connect_copilot(selected, &snapshot.proxy_endpoint)?;
            fleet.update_copilot_status(configured);
        }
        HarnessId::Vscode => {
            let configured = harnesses.connect_vscode(selected, &snapshot.proxy_endpoint)?;
            fleet.update_vscode_status(configured);
        }
    }
    Ok(status(&fleet.snapshot(), harness))
}

fn preferred_model(snapshot: &FleetSnapshot, harness: HarnessId) -> Result<String, String> {
    let current = selected_model(snapshot, harness);
    if current.is_some_and(|selected| model_supports(snapshot, selected, harness.capability())) {
        return Ok(current.unwrap().to_owned());
    }
    for loaded_only in [true, false] {
        for host in snapshot
            .hosts
            .iter()
            .filter(|host| host.connection != ConnectionState::Offline)
        {
            if let Some(model) = host.models.iter().find(|model| {
                model.capabilities.contains(&harness.capability())
                    && (!loaded_only || host.loaded_model_id.as_deref() == Some(model.id.as_str()))
            }) {
                return Ok(format!("{}/{}", host.id, model.id));
            }
        }
    }
    Err(format!(
        "no online model supports {} on this fleet",
        harness.label()
    ))
}

fn model_supports(snapshot: &FleetSnapshot, selected: &str, capability: ProfileCapability) -> bool {
    selected.split_once('/').is_some_and(|(host_id, model_id)| {
        snapshot.hosts.iter().any(|host| {
            host.id == host_id
                && host.connection != ConnectionState::Offline
                && host
                    .models
                    .iter()
                    .any(|model| model.id == model_id && model.capabilities.contains(&capability))
        })
    })
}

fn selected_model(snapshot: &FleetSnapshot, harness: HarnessId) -> Option<&str> {
    match harness {
        HarnessId::Hermes => snapshot.hermes.selected_model.as_deref(),
        HarnessId::HermesCli => snapshot.hermes_cli.selected_model.as_deref(),
        HarnessId::OpenCode | HarnessId::OpenCodeCli => snapshot.opencode.selected_model.as_deref(),
        HarnessId::Codex => snapshot.codex.selected_model.as_deref(),
        HarnessId::ClaudeCode => snapshot.claude_code.selected_model.as_deref(),
        HarnessId::Pi => snapshot.pi.selected_model.as_deref(),
        HarnessId::Copilot => snapshot.copilot.selected_model.as_deref(),
        HarnessId::Vscode => snapshot.vscode.selected_model.as_deref(),
    }
}

fn status(snapshot: &FleetSnapshot, harness: HarnessId) -> HarnessSetupStatus {
    let installed = is_installed(harness);
    let (configured, config_path, selected_model, error) = match harness {
        HarnessId::Hermes => (
            matches!(
                snapshot.hermes.state,
                crate::domain::HermesSyncState::Synced
            ),
            snapshot.hermes.executable_path.clone(),
            snapshot.hermes.selected_model.clone(),
            snapshot.hermes.error.clone(),
        ),
        HarnessId::HermesCli => (
            matches!(
                snapshot.hermes_cli.state,
                crate::domain::HermesSyncState::Synced
            ),
            snapshot.hermes_cli.executable_path.clone(),
            snapshot.hermes_cli.selected_model.clone(),
            snapshot.hermes_cli.error.clone(),
        ),
        HarnessId::OpenCode | HarnessId::OpenCodeCli => (
            matches!(snapshot.opencode.state, OpenCodeSyncState::Synced),
            snapshot.opencode.config_path.clone(),
            snapshot.opencode.selected_model.clone(),
            snapshot.opencode.error.clone(),
        ),
        HarnessId::Codex => harness_status(&snapshot.codex),
        HarnessId::ClaudeCode => harness_status(&snapshot.claude_code),
        HarnessId::Pi => harness_status(&snapshot.pi),
        HarnessId::Copilot => harness_status(&snapshot.copilot),
        HarnessId::Vscode => harness_status(&snapshot.vscode),
    };
    let state = if !installed {
        HarnessSetupState::NotInstalled
    } else if configured {
        HarnessSetupState::Configured
    } else if error.is_some() {
        HarnessSetupState::NeedsRepair
    } else {
        HarnessSetupState::Detected
    };
    HarnessSetupStatus {
        id: harness,
        label: harness.label().to_owned(),
        state,
        config_path,
        selected_model,
        error,
    }
}

fn harness_status(
    status: &crate::domain::HarnessStatus,
) -> (bool, Option<String>, Option<String>, Option<String>) {
    (
        matches!(status.state, HarnessSyncState::Synced),
        status.config_path.clone(),
        status.selected_model.clone(),
        status.error.clone(),
    )
}

fn is_installed(harness: HarnessId) -> bool {
    match harness {
        HarnessId::Hermes | HarnessId::HermesCli => terminal::is_installed(CliHarness::Hermes),
        HarnessId::OpenCode | HarnessId::OpenCodeCli => {
            terminal::is_installed(CliHarness::OpenCode)
        }
        HarnessId::Codex => terminal::is_installed(CliHarness::Codex),
        HarnessId::ClaudeCode => terminal::is_installed(CliHarness::ClaudeCode),
        HarnessId::Pi => terminal::is_installed(CliHarness::Pi),
        HarnessId::Copilot => terminal::is_installed(CliHarness::Copilot),
        HarnessId::Vscode => terminal::vscode_is_installed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{HostStatus, ModelProfile};

    #[test]
    fn preferred_model_uses_a_compatible_loaded_profile_first() {
        let mut snapshot = empty_snapshot();
        snapshot.hosts.push(HostStatus {
            id: "workstation".into(),
            display_name: "WORKSTATION".into(),
            address: "127.0.0.1".into(),
            hardware: "test".into(),
            connection: ConnectionState::Online,
            loaded_model_id: Some("qwen".into()),
            models: vec![ModelProfile {
                id: "qwen".into(),
                display_name: "Qwen".into(),
                runtime: "llama.cpp".into(),
                kind: crate::domain::WorkloadKind::Text,
                capabilities: vec![ProfileCapability::Chat, ProfileCapability::Responses],
                lifecycle_adapter: "llama_swap".into(),
                resource_pool: "default".into(),
                context_length: None,
            }],
            active_requests: 0,
            memory_used_bytes: None,
            memory_total_bytes: None,
            memory_kind: None,
            tokens_per_second: None,
            aggregate_tokens_per_second: None,
            throughput_concurrency: 0,
            last_seen_at_ms: None,
            error: None,
            llama_swap: crate::domain::LlamaSwapStatus::default(),
            channel_gateway: None,
        });
        assert_eq!(
            preferred_model(&snapshot, HarnessId::Pi).unwrap(),
            "workstation/qwen"
        );
    }

    fn empty_snapshot() -> FleetSnapshot {
        FleetSnapshot {
            local_host_id: "workstation".into(),
            config_path: "fleet.json".into(),
            proxy_endpoint: "http://127.0.0.1:38475".into(),
            refreshed_at_ms: 0,
            peer_api: crate::domain::PeerApiStatus::default(),
            hosts: Vec::new(),
            opencode: crate::domain::OpenCodeStatus::default(),
            hermes: crate::domain::HermesStatus::default(),
            hermes_cli: crate::domain::HermesStatus::default(),
            codex: crate::domain::HarnessStatus::default(),
            claude_code: crate::domain::HarnessStatus::default(),
            pi: crate::domain::HarnessStatus::default(),
            copilot: crate::domain::HarnessStatus::default(),
            vscode: crate::domain::HarnessStatus::default(),
        }
    }
}
