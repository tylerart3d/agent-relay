use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ThinkingControls {
    pub adapter: String,
    #[serde(default)]
    pub efforts: Vec<ReasoningEffort>,
    #[serde(default)]
    pub default_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub budget_min: Option<i32>,
    #[serde(default)]
    pub budget_max: Option<i32>,
    #[serde(default)]
    pub budget_step: Option<u32>,
    #[serde(default)]
    pub default_budget: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TemperatureControls {
    pub min: f32,
    pub max: f32,
    pub step: f32,
    #[serde(default)]
    pub default: Option<f32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct InferenceControls {
    #[serde(default)]
    pub thinking: Option<ThinkingControls>,
    #[serde(default)]
    pub temperature: Option<TemperatureControls>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct InferenceOverrides {
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub reasoning_budget: Option<i32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Local,
    Online,
    Offline,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlamaSwapState {
    #[default]
    Unknown,
    Starting,
    Ready,
    Stopped,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct LlamaSwapStatus {
    pub state: LlamaSwapState,
    pub version: String,
    pub endpoint: String,
    pub config_path: String,
    pub pid: Option<u32>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlState {
    Applied,
    Conflict,
    Noop,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ControlOutcome {
    pub state: ControlState,
    pub host_id: String,
    pub active_requests: u32,
    pub loaded_model_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoadModelRequest {
    pub model_id: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UnloadModelsRequest {
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FleetLoadModelRequest {
    pub host_id: String,
    pub model_id: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FleetUnloadModelsRequest {
    pub host_id: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelProfile {
    pub id: String,
    pub display_name: String,
    pub runtime: String,
    #[serde(default)]
    pub kind: WorkloadKind,
    #[serde(default = "default_text_capabilities")]
    pub capabilities: Vec<ProfileCapability>,
    #[serde(default = "default_lifecycle_adapter")]
    pub lifecycle_adapter: String,
    #[serde(default = "default_resource_pool")]
    pub resource_pool: String,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub inference_controls: InferenceControls,
}

impl ModelProfile {
    pub fn supports_capability(&self, capability: &ProfileCapability) -> bool {
        self.kind == WorkloadKind::Text && self.capabilities.contains(capability)
    }

    pub fn supports_any_capability(&self, capabilities: &[ProfileCapability]) -> bool {
        self.kind == WorkloadKind::Text
            && capabilities
                .iter()
                .any(|capability| self.capabilities.contains(capability))
    }

    pub fn supports_openai_generation(&self) -> bool {
        self.supports_any_capability(&[
            ProfileCapability::Chat,
            ProfileCapability::Completions,
            ProfileCapability::Responses,
        ])
    }

    pub fn supports_text_inference(&self) -> bool {
        self.supports_openai_generation()
            || self.supports_capability(&ProfileCapability::AnthropicMessages)
    }

    pub fn validate_inference_override(
        &self,
        inference_override: &InferenceOverrides,
    ) -> Result<(), String> {
        if let Some(effort) = inference_override.reasoning_effort {
            let thinking = self.inference_controls.thinking.as_ref().ok_or_else(|| {
                format!("{} does not expose thinking controls", self.display_name)
            })?;
            if !thinking.efforts.contains(&effort) {
                return Err(format!(
                    "{} does not support reasoning effort {:?}",
                    self.display_name, effort
                ));
            }
        }
        if let Some(budget) = inference_override.reasoning_budget {
            let thinking = self.inference_controls.thinking.as_ref().ok_or_else(|| {
                format!("{} does not expose a reasoning budget", self.display_name)
            })?;
            let (Some(min), Some(max)) = (thinking.budget_min, thinking.budget_max) else {
                return Err(format!(
                    "{} does not expose a reasoning budget",
                    self.display_name
                ));
            };
            if budget < min || budget > max {
                return Err(format!(
                    "reasoning budget for {} must be between {min} and {max}",
                    self.display_name
                ));
            }
            if budget >= 0 {
                let step = thinking.budget_step.unwrap_or(1).max(1) as i32;
                if (budget - min.max(0)) % step != 0 {
                    return Err(format!("reasoning budget must use {step}-token steps"));
                }
            }
        }
        if let Some(temperature) = inference_override.temperature {
            let control = self
                .inference_controls
                .temperature
                .as_ref()
                .ok_or_else(|| {
                    format!("{} does not expose temperature controls", self.display_name)
                })?;
            if !temperature.is_finite() || temperature < control.min || temperature > control.max {
                return Err(format!(
                    "temperature for {} must be between {} and {}",
                    self.display_name, control.min, control.max
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    #[default]
    Text,
    Image,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCapability {
    Chat,
    Completions,
    Responses,
    AnthropicMessages,
    Embeddings,
    VisionInput,
    ImageGeneration,
    WorkflowQueue,
}

fn default_text_capabilities() -> Vec<ProfileCapability> {
    vec![
        ProfileCapability::Chat,
        ProfileCapability::Completions,
        ProfileCapability::Responses,
    ]
}

fn default_lifecycle_adapter() -> String {
    "llama_swap".into()
}

fn default_resource_pool() -> String {
    "default".into()
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HostStatus {
    pub id: String,
    pub display_name: String,
    pub address: String,
    pub hardware: String,
    pub connection: ConnectionState,
    pub models: Vec<ModelProfile>,
    pub loaded_model_id: Option<String>,
    pub active_requests: u32,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    #[serde(default)]
    pub memory_kind: Option<String>,
    pub tokens_per_second: Option<f32>,
    #[serde(default)]
    pub aggregate_tokens_per_second: Option<f32>,
    #[serde(default)]
    pub throughput_concurrency: u32,
    pub last_seen_at_ms: Option<u64>,
    pub error: Option<String>,
    #[serde(default)]
    pub llama_swap: LlamaSwapStatus,
    #[serde(default)]
    pub channel_gateway: Option<GatewayRuntimeStatus>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FleetSnapshot {
    pub local_host_id: String,
    pub config_path: String,
    pub proxy_endpoint: String,
    pub refreshed_at_ms: u64,
    pub peer_api: PeerApiStatus,
    pub hosts: Vec<HostStatus>,
    pub opencode: OpenCodeStatus,
    pub hermes: HermesStatus,
    pub hermes_cli: HermesStatus,
    pub codex: HarnessStatus,
    pub claude_code: HarnessStatus,
    pub pi: HarnessStatus,
    pub copilot: HarnessStatus,
    pub vscode: HarnessStatus,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSyncState {
    #[default]
    Disabled,
    Pending,
    Synced,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct HarnessStatus {
    pub state: HarnessSyncState,
    pub config_path: Option<String>,
    pub selected_model: Option<String>,
    pub last_synced_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerApiState {
    #[default]
    Starting,
    Listening,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PeerApiStatus {
    pub state: PeerApiState,
    pub address: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenCodeSyncState {
    #[default]
    Disabled,
    Pending,
    Synced,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct OpenCodeStatus {
    pub state: OpenCodeSyncState,
    pub config_path: Option<String>,
    pub model_count: usize,
    pub selected_model: Option<String>,
    pub last_synced_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HermesSyncState {
    Disabled,
    #[default]
    Pending,
    Synced,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct HermesStatus {
    pub state: HermesSyncState,
    pub executable_path: Option<String>,
    pub selected_model: Option<String>,
    pub last_synced_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PeerStatusResponse {
    #[serde(default)]
    pub protocol: Option<String>,
    pub host_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub hardware: Option<String>,
    pub models: Vec<ModelProfile>,
    pub loaded_model_id: Option<String>,
    #[serde(default)]
    pub active_requests: u32,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    #[serde(default)]
    pub memory_kind: Option<String>,
    pub tokens_per_second: Option<f32>,
    #[serde(default)]
    pub aggregate_tokens_per_second: Option<f32>,
    #[serde(default)]
    pub throughput_concurrency: u32,
    #[serde(default)]
    pub llama_swap: LlamaSwapStatus,
    #[serde(default)]
    pub channel_gateway: Option<GatewayRuntimeStatus>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayRuntimeState {
    Starting,
    Standby,
    Active,
    NeedsCredentials,
    Error,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GatewayRuntimeStatus {
    pub state: GatewayRuntimeState,
    pub host_id: String,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_profiles_default_to_text_llama_swap_profiles() {
        let profile: ModelProfile = serde_json::from_value(serde_json::json!({
            "id": "qwen",
            "display_name": "Qwen",
            "runtime": "llama_cpp"
        }))
        .expect("legacy profile");

        assert_eq!(profile.kind, WorkloadKind::Text);
        assert_eq!(profile.lifecycle_adapter, "llama_swap");
        assert_eq!(profile.resource_pool, "default");
        assert!(profile.supports_text_inference());
    }

    #[test]
    fn image_profiles_are_not_text_client_targets() {
        let profile = ModelProfile {
            id: "sdxl".into(),
            display_name: "SDXL".into(),
            runtime: "comfyui".into(),
            kind: WorkloadKind::Image,
            capabilities: vec![ProfileCapability::ImageGeneration],
            lifecycle_adapter: "comfyui".into(),
            resource_pool: "gpu0".into(),
            context_length: None,
            inference_controls: InferenceControls::default(),
        };

        assert!(!profile.supports_text_inference());
    }

    #[test]
    fn anthropic_only_profiles_are_generative_text_targets() {
        let profile = ModelProfile {
            id: "claude-compatible".into(),
            display_name: "Claude-compatible".into(),
            runtime: "test".into(),
            kind: WorkloadKind::Text,
            capabilities: vec![ProfileCapability::AnthropicMessages],
            lifecycle_adapter: "test".into(),
            resource_pool: "default".into(),
            context_length: None,
            inference_controls: InferenceControls::default(),
        };

        assert!(profile.supports_text_inference());
        assert!(profile.supports_capability(&ProfileCapability::AnthropicMessages));
        assert!(!profile.supports_openai_generation());
    }

    #[test]
    fn embeddings_only_profiles_are_not_generative_text_targets() {
        let profile = ModelProfile {
            id: "embedding".into(),
            display_name: "Embedding".into(),
            runtime: "test".into(),
            kind: WorkloadKind::Text,
            capabilities: vec![ProfileCapability::Embeddings],
            lifecycle_adapter: "test".into(),
            resource_pool: "default".into(),
            context_length: None,
            inference_controls: InferenceControls::default(),
        };

        assert!(!profile.supports_text_inference());
        assert!(!profile.supports_openai_generation());
        assert!(profile.supports_capability(&ProfileCapability::Embeddings));
    }

    #[test]
    fn fleet_control_requests_default_force_to_false() {
        let load: FleetLoadModelRequest = serde_json::from_value(serde_json::json!({
            "host_id": "workstation",
            "model_id": "qwen"
        }))
        .expect("load request");
        let unload: FleetUnloadModelsRequest = serde_json::from_value(serde_json::json!({
            "host_id": "workstation"
        }))
        .expect("unload request");

        assert!(!load.force);
        assert_eq!(load.context_window, None);
        assert!(!unload.force);
    }

    #[test]
    fn fleet_load_requests_preserve_an_explicit_context_window() {
        let load: FleetLoadModelRequest = serde_json::from_value(serde_json::json!({
            "host_id": "workstation",
            "model_id": "qwen",
            "context_window": 262144
        }))
        .expect("context-aware load request");

        assert_eq!(load.context_window, Some(262_144));
    }
}
