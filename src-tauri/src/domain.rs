use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Off,
    On,
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
    /// Reasoning-token limit for each effort level. Levels not listed use
    /// `standard_effort_budget`, clipped to the model's budget range.
    #[serde(default)]
    pub effort_budgets: std::collections::BTreeMap<ReasoningEffort, i32>,
}

impl ThinkingControls {
    /// The fixed reasoning-token limit for an effort level, or `None` when the
    /// model has no request-level budget. Off is handled by each adapter.
    pub fn budget_for(&self, effort: Option<ReasoningEffort>) -> Option<i32> {
        let Some(effort) = effort else {
            return self.default_budget;
        };
        if effort == ReasoningEffort::Off {
            return None;
        }
        if let Some(budget) = self.effort_budgets.get(&effort) {
            return Some(*budget);
        }
        let (min, max) = (self.budget_min?, self.budget_max?);
        let unlimited = if self.adapter == "strata" {
            0
        } else if min < 0 {
            -1
        } else {
            max
        };
        Some(match effort {
            ReasoningEffort::Off => unreachable!(),
            ReasoningEffort::On => self.default_budget.unwrap_or(unlimited),
            ReasoningEffort::Minimal => 512.clamp(min.max(1), max),
            ReasoningEffort::Low => 2_048.clamp(min.max(1), max),
            ReasoningEffort::Medium => 8_192.clamp(min.max(1), max),
            ReasoningEffort::High => 16_384.clamp(min.max(1), max),
            ReasoningEffort::Xhigh | ReasoningEffort::Max => unlimited,
        })
    }
}

/// The sampling temperature is part of a model's profile, not a client setting.
/// Older profiles also carry a min/max/step range; only `default` is used.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TemperatureControls {
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfilePriority {
    Background,
    #[default]
    Foreground,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelProfile {
    #[serde(default)]
    pub priority: ProfilePriority,
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
    pub parallel_slots: Option<u32>,
    #[serde(default)]
    pub inference_controls: InferenceControls,
}

impl ModelProfile {
    /// Explicit opt-in for an embedding service in an isolated, non-exclusive swap group.
    pub fn is_independent_embedding_service(&self) -> bool {
        self.kind == WorkloadKind::Text
            && matches!(self.resource_pool.as_str(), "cpu" | "embeddings")
            && self.capabilities == [ProfileCapability::Embeddings]
    }

    /// Runs alongside the host's one foreground model instead of occupying it:
    /// isolated embedding services, and worker services (for example text-to-speech)
    /// placed in their own resource pool and non-exclusive llama-swap group. Such a
    /// service never blocks Auto, background OCR, or the idle-foreground release.
    pub fn is_independent_service(&self) -> bool {
        self.is_independent_embedding_service()
            || (self.is_worker_service() && !matches!(self.resource_pool.as_str(), "" | "default"))
    }

    /// A worker that serves OpenAI-style speech routes.
    pub fn is_speech_worker(&self, capability: &ProfileCapability) -> bool {
        self.is_worker_service() && self.capabilities.contains(capability)
    }

    pub fn is_voice_service(&self) -> bool {
        self.kind == WorkloadKind::Voice
            && self
                .capabilities
                .contains(&ProfileCapability::RealtimeAudio)
    }

    /// True for supervised HTTP services reachable through the worker proxy routes.
    pub fn is_worker_service(&self) -> bool {
        self.kind == WorkloadKind::Worker
            && self.capabilities.contains(&ProfileCapability::HttpService)
    }

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
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    #[default]
    Text,
    Image,
    /// Full-duplex speech; never offered to text-only harnesses.
    Voice,
    /// A supervised HTTP service that is not a language model (for example a vision
    /// worker). Reached only through the bounded `/api/worker/...` routes.
    Worker,
    /// Any kind this build does not know. Keeps peer status decoding forward-compatible.
    #[serde(other)]
    Unknown,
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
    /// Generic HTTP service exposed through the worker proxy routes.
    HttpService,
    RealtimeAudio,
    /// Worker answers OpenAI-style `/v1/audio/speech` (and voice listing).
    TextToSpeech,
    /// Worker answers OpenAI-style `/v1/audio/transcriptions`.
    SpeechToText,
    #[serde(other)]
    Unknown,
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
    #[serde(default)]
    pub gpu_activity: Option<crate::gpu_activity::GpuActivity>,
    #[serde(default = "default_available")]
    pub available: bool,
    #[serde(default)]
    pub routing_version: u8,
    #[serde(default)]
    pub reclaim: crate::admission::ReclaimStatus,
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
    #[serde(default)]
    pub openbot: HarnessStatus,
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
    pub gpu_activity: Option<crate::gpu_activity::GpuActivity>,
    #[serde(default = "default_available")]
    pub available: bool,
    #[serde(default)]
    pub routing_version: u8,
    #[serde(default)]
    pub reclaim: crate::admission::ReclaimStatus,
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
}

fn default_available() -> bool {
    true
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
            priority: Default::default(),
            id: "sdxl".into(),
            display_name: "SDXL".into(),
            runtime: "comfyui".into(),
            kind: WorkloadKind::Image,
            capabilities: vec![ProfileCapability::ImageGeneration],
            lifecycle_adapter: "comfyui".into(),
            resource_pool: "gpu0".into(),
            context_length: None,
            parallel_slots: None,
            inference_controls: InferenceControls::default(),
        };

        assert!(!profile.supports_text_inference());
    }

    #[test]
    fn worker_profiles_are_services_not_text_targets() {
        let profile = ModelProfile {
            priority: Default::default(),
            id: "memory-vision".into(),
            display_name: "memory vision worker".into(),
            runtime: "python".into(),
            kind: WorkloadKind::Worker,
            capabilities: vec![ProfileCapability::HttpService],
            lifecycle_adapter: "llama_swap".into(),
            resource_pool: "gpu0".into(),
            context_length: None,
            parallel_slots: None,
            inference_controls: InferenceControls::default(),
        };

        assert!(profile.is_worker_service());
        assert!(!profile.supports_text_inference());
        assert!(!profile.supports_capability(&ProfileCapability::HttpService));
    }

    #[test]
    fn workers_in_their_own_pool_coexist_with_the_foreground_model() {
        let mut tts = ModelProfile {
            priority: Default::default(),
            id: "kokoro-tts".into(),
            display_name: "Kokoro TTS".into(),
            runtime: "coreml".into(),
            kind: WorkloadKind::Worker,
            capabilities: vec![
                ProfileCapability::HttpService,
                ProfileCapability::TextToSpeech,
            ],
            lifecycle_adapter: "llama_swap".into(),
            resource_pool: "speech".into(),
            context_length: None,
            parallel_slots: None,
            inference_controls: InferenceControls::default(),
        };
        assert!(tts.is_independent_service());
        assert!(tts.is_speech_worker(&ProfileCapability::TextToSpeech));
        assert!(!tts.is_speech_worker(&ProfileCapability::SpeechToText));
        // In the shared default pool a worker still occupies the host.
        tts.resource_pool = "default".into();
        assert!(!tts.is_independent_service());
        // Text models never count as independent services by pool alone.
        let mut text = tts.clone();
        text.kind = WorkloadKind::Text;
        text.capabilities = vec![ProfileCapability::Chat];
        text.resource_pool = "speech".into();
        assert!(!text.is_independent_service());
    }

    #[test]
    fn reasoning_limits_are_fixed_per_effort_level() {
        let llama: ThinkingControls = serde_json::from_value(serde_json::json!({
            "adapter": "llama_cpp",
            "efforts": ["off", "low", "medium", "xhigh"],
            "budget_min": -1, "budget_max": 16384, "default_budget": 2048,
            "effort_budgets": {"medium": 6000}
        }))
        .unwrap();
        assert_eq!(llama.budget_for(Some(ReasoningEffort::Off)), None);
        assert_eq!(llama.budget_for(Some(ReasoningEffort::Low)), Some(2_048));
        // A profile can set its own limit for a level.
        assert_eq!(llama.budget_for(Some(ReasoningEffort::Medium)), Some(6_000));
        assert_eq!(llama.budget_for(Some(ReasoningEffort::Xhigh)), Some(-1));
        assert_eq!(llama.budget_for(None), Some(2_048));

        let strata: ThinkingControls = serde_json::from_value(serde_json::json!({
            "adapter": "strata", "budget_min": 0, "budget_max": 4096
        }))
        .unwrap();
        // Clipped to the model's maximum; Strata spells unlimited as 0.
        assert_eq!(strata.budget_for(Some(ReasoningEffort::High)), Some(4_096));
        assert_eq!(strata.budget_for(Some(ReasoningEffort::Max)), Some(0));

        let toggle: ThinkingControls =
            serde_json::from_value(serde_json::json!({"adapter": "mtplx"})).unwrap();
        assert_eq!(toggle.budget_for(Some(ReasoningEffort::On)), None);

        // Older profiles still carry a temperature range; only the value is kept.
        let temperature: TemperatureControls = serde_json::from_value(
            serde_json::json!({"min": 0.0, "max": 2.0, "step": 0.05, "default": 0.3}),
        )
        .unwrap();
        assert_eq!(temperature.default, Some(0.3));
        // Saved overrides from older versions drop their removed fields.
        let saved: InferenceOverrides = serde_json::from_value(serde_json::json!({
            "reasoning_effort": "high", "reasoning_budget": 4096, "temperature": 0.5
        }))
        .unwrap();
        assert_eq!(saved.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn speech_capabilities_round_trip_and_unknown_ones_stay_decodable() {
        let caps: Vec<ProfileCapability> =
            serde_json::from_str(r#"["text_to_speech","speech_to_text","future_thing"]"#).unwrap();
        assert_eq!(
            caps,
            [
                ProfileCapability::TextToSpeech,
                ProfileCapability::SpeechToText,
                ProfileCapability::Unknown
            ]
        );
    }

    #[test]
    fn unknown_kinds_decode_for_forward_compatibility() {
        let profile: ModelProfile = serde_json::from_value(serde_json::json!({
            "id": "future",
            "display_name": "Future",
            "runtime": "x",
            "kind": "hologram"
        }))
        .expect("unknown kind decodes");

        assert_eq!(profile.kind, WorkloadKind::Unknown);
        assert!(!profile.supports_text_inference());
        assert!(!profile.is_worker_service());
    }

    #[test]
    fn anthropic_only_profiles_are_generative_text_targets() {
        let profile = ModelProfile {
            priority: Default::default(),
            id: "claude-compatible".into(),
            display_name: "Claude-compatible".into(),
            runtime: "test".into(),
            kind: WorkloadKind::Text,
            capabilities: vec![ProfileCapability::AnthropicMessages],
            lifecycle_adapter: "test".into(),
            resource_pool: "default".into(),
            context_length: None,
            parallel_slots: None,
            inference_controls: InferenceControls::default(),
        };

        assert!(profile.supports_text_inference());
        assert!(profile.supports_capability(&ProfileCapability::AnthropicMessages));
        assert!(!profile.supports_openai_generation());
    }

    #[test]
    fn embeddings_only_profiles_are_not_generative_text_targets() {
        let profile = ModelProfile {
            priority: Default::default(),
            id: "embedding".into(),
            display_name: "Embedding".into(),
            runtime: "test".into(),
            kind: WorkloadKind::Text,
            capabilities: vec![ProfileCapability::Embeddings],
            lifecycle_adapter: "test".into(),
            resource_pool: "default".into(),
            context_length: None,
            parallel_slots: None,
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
