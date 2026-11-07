//! Explicit, capability-filtered Auto routing. No prompt classification model is needed.
use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{ConnectionState, FleetSnapshot, ProfileCapability, WorkloadKind};

pub const CLIENTS: &[&str] = &[
    "hermes",
    "hermes_cli",
    "opencode",
    "openbot",
    "codex",
    "claude_code",
    "pi",
    "copilot",
    "vscode",
];

pub fn canonical_client(client: &str) -> &str {
    if client == "opencode_cli" {
        "opencode"
    } else {
        client
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    #[default]
    Pinned,
    Auto,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApprovedModel {
    pub model: String,
    /// Explicit operator approval for tool calling; not inferred from a model name.
    #[serde(default)]
    pub tools: bool,
}

fn default_context() -> u32 {
    65_536
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClientPolicy {
    #[serde(default)]
    pub mode: RouteMode,
    /// None follows the shared list; an empty list deliberately permits nothing.
    #[serde(default)]
    pub models: Option<Vec<ApprovedModel>>,
    #[serde(default = "default_context")]
    pub minimum_context: u32,
}

impl Default for ClientPolicy {
    fn default() -> Self {
        Self {
            mode: RouteMode::Pinned,
            models: None,
            minimum_context: default_context(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub models: Vec<ApprovedModel>,
    #[serde(default)]
    pub clients: BTreeMap<String, ClientPolicy>,
}

impl RoutingConfig {
    pub fn policy(&self, client: &str) -> ClientPolicy {
        self.clients
            .get(canonical_client(client))
            .cloned()
            .unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), String> {
        for (client, policy) in &self.clients {
            if !CLIENTS.contains(&client.as_str()) {
                return Err(format!("Unknown routing client: {client}"));
            }
            if !(16_384..=1_048_576).contains(&policy.minimum_context) {
                return Err("Auto context must be between 16K and 1M tokens".into());
            }
        }
        for models in std::iter::once(&self.models)
            .chain(self.clients.values().filter_map(|p| p.models.as_ref()))
        {
            if models.len() > 128 {
                return Err("Approve at most 128 models per list".into());
            }
            let mut seen = HashSet::new();
            for entry in models {
                if !entry
                    .model
                    .split_once('/')
                    .is_some_and(|(h, m)| !h.is_empty() && !m.is_empty())
                    || entry.model.len() > 512
                    || entry.model.chars().any(char::is_control)
                    || !seen.insert(&entry.model)
                {
                    return Err("Approved models must be unique host/model identifiers".into());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Requirements {
    pub capability: ProfileCapability,
    pub vision: bool,
    pub tools: bool,
    pub context: u32,
}

/// Admission heuristic, not tokenizer output. Counting every UTF-8 byte as a
/// token rejected ordinary tool conversations long before their context filled.
/// Use three ASCII characters per token (with headroom over typical prose), but
/// retain a byte reservation for non-ASCII text whose tokenization varies more.
fn estimate_text_tokens(text: &str) -> u64 {
    let ascii = text.bytes().filter(u8::is_ascii).count() as u64;
    ascii.div_ceil(3) + (text.len() as u64 - ascii)
}

impl Requirements {
    pub fn parse(path: &str, body: &[u8], minimum_context: u32) -> Result<Self, String> {
        let capability =
            match path {
                "chat/completions" => ProfileCapability::Chat,
                "completions" => ProfileCapability::Completions,
                "responses" => ProfileCapability::Responses,
                "messages" => ProfileCapability::AnthropicMessages,
                _ => return Err(
                    "Auto supports chat, completions, Responses and Messages; pin other workloads"
                        .into(),
                ),
            };
        let value: Value =
            serde_json::from_slice(body).map_err(|e| format!("Invalid JSON: {e}"))?;
        let mut vision = false;
        let mut unsupported = false;
        fn inspect(v: &Value, vision: &mut bool, unsupported: &mut bool) -> u64 {
            match v {
                Value::Object(o) => {
                    let kind = o.get("type").and_then(Value::as_str).unwrap_or("");
                    if ["image_url", "input_image", "image"].contains(&kind) {
                        *vision = true;
                        // Conservative reservation; exact image tokenization belongs to the runtime.
                        return 16_384;
                    }
                    if [
                        "input_audio",
                        "audio",
                        "video",
                        "input_file",
                        "file",
                        "document",
                    ]
                    .contains(&kind)
                    {
                        *unsupported = true;
                    }
                    // Schema/property names and JSON framing also consume tokens.
                    2 + o
                        .iter()
                        .map(|(key, value)| {
                            estimate_text_tokens(key) + 2 + inspect(value, vision, unsupported)
                        })
                        .sum::<u64>()
                }
                Value::Array(a) => {
                    2 + a
                        .iter()
                        .map(|v| inspect(v, vision, unsupported))
                        .sum::<u64>()
                }
                Value::String(s) => estimate_text_tokens(s) + 2,
                Value::Null => 1,
                Value::Bool(_) => 1,
                Value::Number(n) => estimate_text_tokens(&n.to_string()),
            }
        }
        let input = inspect(&value, &mut vision, &mut unsupported);
        if unsupported {
            return Err("Auto cannot verify this media type; choose a pinned model".into());
        }
        let output = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
            .iter()
            .filter_map(|k| value.get(k).and_then(Value::as_u64))
            .max()
            .unwrap_or(8192);
        let context = input
            .saturating_add(output)
            .saturating_add(2048)
            .max(minimum_context as u64);
        let context = u32::try_from(context).map_err(|_| "Request exceeds supported context")?;
        let tools = value
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty())
            || value
                .get("functions")
                .and_then(Value::as_array)
                .is_some_and(|t| !t.is_empty());
        Ok(Self {
            capability,
            vision,
            tools,
            context,
        })
    }
}

fn candidate_rank(
    entry: &ApprovedModel,
    snapshot: &FleetSnapshot,
    needs: &Requirements,
    previous: Option<&str>,
) -> Result<u8, String> {
    let Some((host_id, model_id)) = entry.model.split_once('/') else {
        return Err("invalid host/model identifier".into());
    };
    let Some(host) = snapshot.hosts.iter().find(|h| h.id == host_id) else {
        return Err("host not discovered".into());
    };
    if !host.available {
        return Err("machine disabled by operator".into());
    }
    if host.connection == ConnectionState::Offline {
        return Err("machine offline".into());
    }
    if host.routing_version < 2 {
        return Err("machine needs Agent Relay 2 or newer".into());
    }
    if host
        .gpu_activity
        .as_ref()
        .is_some_and(|g| g.blocks_auto(crate::fleet::now_ms()))
    {
        return Err("GPU busy with another application".into());
    }
    let Some(model) = host.models.iter().find(|m| m.id == model_id) else {
        return Err("profile not found".into());
    };
    if model.kind != WorkloadKind::Text || !model.capabilities.contains(&needs.capability) {
        return Err("request API not supported".into());
    }
    if needs.vision && !model.capabilities.contains(&ProfileCapability::VisionInput) {
        return Err("vision not supported".into());
    }
    if needs.tools && !entry.tools {
        return Err("tools not approved in Auto settings".into());
    }
    let Some(context) = model.context_length else {
        return Err("context capacity not declared".into());
    };
    if context < needs.context {
        return Err(format!(
            "context capacity {context} is below admission estimate/minimum {} tokens",
            needs.context
        ));
    }
    let loaded = host.loaded_model_id.as_deref() == Some(model_id);
    let background_only = host.loaded_model_id.as_deref().is_some_and(|id| {
        host.models
            .iter()
            .any(|m| m.id == id && m.is_cpu_embedding_service())
    });
    if !loaded && !background_only && (host.loaded_model_id.is_some() || host.active_requests != 0)
    {
        return Err("machine occupied; Auto will not replace its model".into());
    }
    let tier = if loaded && previous == Some(entry.model.as_str()) {
        0
    } else if loaded {
        1
    } else {
        2
    };
    Ok(tier)
}

/// Loaded candidates precede cold candidates. Within each tier, preserve approval order.
/// A previous choice remains sticky while it is still eligible and loaded.
pub fn candidates(
    config: &RoutingConfig,
    client: &str,
    snapshot: &FleetSnapshot,
    needs: &Requirements,
    previous: Option<&str>,
) -> Vec<String> {
    let policy = config.policy(client);
    let approved = policy.models.as_ref().unwrap_or(&config.models);
    let mut eligible = Vec::new();
    for (index, entry) in approved.iter().enumerate() {
        if let Ok(tier) = candidate_rank(entry, snapshot, needs, previous) {
            eligible.push((tier, index, entry.model.clone()));
        }
    }
    eligible.sort_by_key(|(tier, index, _)| (*tier, *index));
    eligible.into_iter().map(|(_, _, model)| model).collect()
}

/// Bounded configuration-only diagnostics. Never include request text or tool data.
pub fn rejection_message(
    config: &RoutingConfig,
    client: &str,
    snapshot: &FleetSnapshot,
    needs: &Requirements,
) -> String {
    let policy = config.policy(client);
    let approved = policy.models.as_ref().unwrap_or(&config.models);
    if approved.is_empty() {
        return "No models approved for this client. Add models in Auto routing settings.".into();
    }
    let mut reasons = approved
        .iter()
        .take(8)
        .filter_map(|entry| {
            candidate_rank(entry, snapshot, needs, None)
                .err()
                .map(|reason| format!("{}: {reason}", entry.model))
        })
        .collect::<Vec<_>>();
    if approved.len() > 8 {
        reasons.push(format!(
            "{} further approved models omitted",
            approved.len() - 8
        ));
    }
    format!(
        "No approved available model meets this request. {}. Check Auto routing settings.",
        reasons.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::FleetConfig, fleet::FleetService};

    fn fixture() -> (RoutingConfig, FleetSnapshot, Requirements) {
        let service = FleetService::new(
            FleetConfig::defaults("WORKSTATION"),
            std::env::temp_dir().join(format!("relay-routing-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let mut snapshot = service.snapshot();
        snapshot.hosts.retain(|h| h.id == snapshot.local_host_id);
        let host = &mut snapshot.hosts[0];
        host.id = "desktop".into();
        host.routing_version = 2;
        host.models = vec![serde_json::from_value(serde_json::json!({
            "id":"qwen", "display_name":"Qwen", "runtime":"llama.cpp", "context_length":131072,
            "capabilities":["chat","responses","vision_input"]
        }))
        .unwrap()];
        let mut second = host.clone();
        second.id = "laptop".into();
        second.connection = ConnectionState::Online;
        second.loaded_model_id = Some("qwen".into());
        snapshot.hosts.push(second);
        let config = RoutingConfig {
            models: vec![
                ApprovedModel {
                    model: "desktop/qwen".into(),
                    tools: true,
                },
                ApprovedModel {
                    model: "laptop/qwen".into(),
                    tools: true,
                },
            ],
            clients: BTreeMap::new(),
        };
        let needs = Requirements {
            capability: ProfileCapability::Chat,
            vision: false,
            tools: true,
            context: 65536,
        };
        (config, snapshot, needs)
    }

    #[test]
    fn loaded_first_then_sticky_then_approved_order() {
        let (config, mut snapshot, needs) = fixture();
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, None),
            ["laptop/qwen", "desktop/qwen"]
        );
        snapshot.hosts[0].loaded_model_id = Some("qwen".into());
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, None)[0],
            "desktop/qwen"
        );
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, Some("laptop/qwen"))[0],
            "laptop/qwen"
        );
    }

    #[test]
    fn unavailable_offline_and_older_peers_are_excluded() {
        let (config, mut snapshot, needs) = fixture();
        snapshot.hosts[0].available = false;
        snapshot.hosts[1].connection = ConnectionState::Offline;
        assert!(candidates(&config, "hermes", &snapshot, &needs, None).is_empty());
        snapshot.hosts[1].connection = ConnectionState::Online;
        snapshot.hosts[1].routing_version = 0;
        assert!(candidates(&config, "hermes", &snapshot, &needs, None).is_empty());
    }

    #[test]
    fn outside_gpu_load_excludes_even_a_loaded_sticky_model() {
        let (config, mut snapshot, needs) = fixture();
        snapshot.hosts[1].gpu_activity = Some(crate::gpu_activity::GpuActivity {
            busy: true,
            external_percent: Some(85.0),
            sampled_at_ms: crate::fleet::now_ms(),
        });
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, Some("laptop/qwen")),
            ["desktop/qwen"]
        );
        snapshot.hosts[1]
            .gpu_activity
            .as_mut()
            .unwrap()
            .sampled_at_ms = 0;
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, None)[0],
            "laptop/qwen"
        );
    }

    #[test]
    fn cold_start_never_evicts_or_interrupts_busy_hosts() {
        let (config, mut snapshot, needs) = fixture();
        snapshot.hosts[0].loaded_model_id = Some("other".into());
        snapshot.hosts[1].loaded_model_id = None;
        snapshot.hosts[1].active_requests = 1;
        assert!(candidates(&config, "hermes", &snapshot, &needs, None).is_empty());
        snapshot.hosts[1].active_requests = 0;
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, None),
            ["laptop/qwen"]
        );
    }

    #[test]
    fn cpu_embeddings_do_not_make_an_idle_gpu_ineligible() {
        let (config, mut snapshot, needs) = fixture();
        snapshot.hosts[0].models.push(
            serde_json::from_value(serde_json::json!({
                "id":"embed", "display_name":"Embedding", "runtime":"llama.cpp",
                "resource_pool":"cpu", "capabilities":["embeddings"]
            }))
            .unwrap(),
        );
        snapshot.hosts[0].loaded_model_id = Some("embed".into());
        snapshot.hosts[0].active_requests = 1;
        assert!(
            candidates(&config, "hermes", &snapshot, &needs, None).contains(&"desktop/qwen".into())
        );
    }

    #[test]
    fn request_capabilities_and_context_must_be_explicitly_supported() {
        let (mut config, mut snapshot, mut needs) = fixture();
        config.models[0].tools = false;
        snapshot.hosts[1].models[0].context_length = None;
        assert!(candidates(&config, "codex", &snapshot, &needs, None).is_empty());
        config.models[0].tools = true;
        needs.vision = true;
        snapshot.hosts[0].models[0]
            .capabilities
            .retain(|c| *c != ProfileCapability::VisionInput);
        assert!(candidates(&config, "hermes", &snapshot, &needs, None).is_empty());
        needs.vision = false;
        needs.capability = ProfileCapability::AnthropicMessages;
        assert!(candidates(&config, "claude_code", &snapshot, &needs, None).is_empty());
        needs.capability = ProfileCapability::Chat;
        needs.context = 262144;
        assert!(candidates(&config, "hermes", &snapshot, &needs, None).is_empty());
    }

    #[test]
    fn empty_client_override_does_not_fall_back_to_shared_list() {
        let (mut config, snapshot, needs) = fixture();
        config.clients.insert(
            "opencode".into(),
            ClientPolicy {
                mode: RouteMode::Auto,
                models: Some(vec![]),
                minimum_context: 65536,
            },
        );
        assert!(candidates(&config, "opencode_cli", &snapshot, &needs, None).is_empty());
        assert_eq!(
            candidates(&config, "hermes", &snapshot, &needs, None).len(),
            2
        );
        config.models.push(config.models[0].clone());
        assert!(config.validate().is_err());
    }

    #[test]
    fn request_parser_checks_media_tool_schemas_and_output_reservation() {
        let needs = Requirements::parse("chat/completions", br#"{"messages":[{"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,xyz"}}]}],"tools":[{}],"max_tokens":32768}"#,16384).unwrap();
        assert!(needs.vision && needs.tools);
        assert!(needs.context >= 16384 + 32768);
        assert!(Requirements::parse(
            "chat/completions",
            br#"{"messages":[{"type":"input_audio"}]}"#,
            16384
        )
        .is_err());
        assert!(Requirements::parse("embeddings", b"{}", 65536).is_err());
        assert!(Requirements::parse("responses", b"invalid", 65536).is_err());
    }

    #[test]
    fn ordinary_tool_history_does_not_count_every_byte_as_a_token() {
        let (config, snapshot, _) = fixture();
        // A document/tool conversation that the old byte upper bound rejected
        // at 128K despite fitting comfortably with a 32K output reservation.
        let body = serde_json::to_vec(&serde_json::json!({
            "messages": [
                {"role":"user", "content":"Summarize this document. ".repeat(4000)},
                {"role":"assistant", "tool_calls":[{"id":"synthetic", "type":"function", "function":{"name":"lookup", "arguments":"{}"}}]},
                {"role":"tool", "tool_call_id":"synthetic", "content":"The lookup returned this document entry. ".repeat(1300)}
            ],
            "tools":[{"type":"function", "function":{"name":"lookup", "parameters":{"type":"object","properties":{}}}}],
            "max_tokens":32768
        })).unwrap();
        assert!(body.len() > 131072);
        let needs = Requirements::parse("chat/completions", &body, 65536).unwrap();
        assert!(needs.tools);
        assert!((65536..100000).contains(&needs.context));
        assert_eq!(
            candidates(&config, "openbot", &snapshot, &needs, None)[0],
            "laptop/qwen"
        );
    }

    #[test]
    fn context_guard_still_reserves_output_minimum_unicode_and_schema_keys() {
        let (config, snapshot, _) = fixture();
        for body in [
            serde_json::json!({"messages":[{"content":"plain text ".repeat(50000)}]}),
            serde_json::json!({"messages":[], "max_tokens":131072}),
            serde_json::json!({"messages":[{"content":"漢".repeat(50000)}]}),
            serde_json::json!({"tools":[{"parameters":{"properties":{ "x".repeat(400000): {"type":"string"}}}}]}),
        ] {
            let needs = Requirements::parse(
                "chat/completions",
                &serde_json::to_vec(&body).unwrap(),
                65536,
            )
            .unwrap();
            assert!(candidates(&config, "openbot", &snapshot, &needs, None).is_empty());
        }
        let needs = Requirements::parse("chat/completions", b"{}", 262144).unwrap();
        assert_eq!(needs.context, 262144);
        assert!(candidates(&config, "openbot", &snapshot, &needs, None).is_empty());
    }

    #[test]
    fn encoded_images_do_not_count_as_text() {
        let body = |url: &str| {
            serde_json::to_vec(&serde_json::json!({"messages":[{"content":[{"type":"image_url", "image_url":{"url":url}}]}]})).unwrap()
        };
        let short = Requirements::parse(
            "chat/completions",
            &body("data:image/png;base64,xxx"),
            16384,
        )
        .unwrap();
        let large = Requirements::parse(
            "chat/completions",
            &body(&format!("data:image/png;base64,{}", "x".repeat(500000))),
            16384,
        )
        .unwrap();
        assert!(large.vision);
        assert_eq!(large.context, short.context);
    }

    #[test]
    fn rejections_explain_gpu_and_context_without_request_content() {
        let (config, mut snapshot, mut needs) = fixture();
        snapshot.hosts[0].gpu_activity = Some(crate::gpu_activity::GpuActivity {
            busy: true,
            external_percent: Some(85.0),
            sampled_at_ms: crate::fleet::now_ms(),
        });
        needs.context = 150000;
        let message = rejection_message(&config, "openbot", &snapshot, &needs);
        assert!(message.contains("desktop/qwen: GPU busy"));
        assert!(message.contains("laptop/qwen: context capacity 131072"));
        assert!(message.contains("150000"));
        assert!(candidates(&config, "openbot", &snapshot, &needs, None).is_empty());
        assert!(
            rejection_message(&RoutingConfig::default(), "openbot", &snapshot, &needs)
                .contains("No models approved")
        );
    }
}
