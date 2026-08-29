use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::{
    config::{self, HarnessConfig, DEFAULT_CLIENT_CONTEXT_WINDOW},
    domain::{FleetSnapshot, HarnessStatus, HarnessSyncState},
};

const PROVIDER_ID: &str = "agentrelay";

pub struct LocalHarnessIntegrations {
    codex: RwLock<HarnessConfig>,
    claude_code: RwLock<HarnessConfig>,
    pi: RwLock<HarnessConfig>,
    copilot: RwLock<HarnessConfig>,
    vscode: RwLock<HarnessConfig>,
    fleet_config_dir: PathBuf,
}

impl LocalHarnessIntegrations {
    pub fn new(
        codex: HarnessConfig,
        claude_code: HarnessConfig,
        pi: HarnessConfig,
        copilot: HarnessConfig,
        vscode: HarnessConfig,
        fleet_config_dir: PathBuf,
    ) -> Self {
        Self {
            codex: RwLock::new(codex),
            claude_code: RwLock::new(claude_code),
            pi: RwLock::new(pi),
            copilot: RwLock::new(copilot),
            vscode: RwLock::new(vscode),
            fleet_config_dir,
        }
    }

    pub fn codex_status(&self) -> HarnessStatus {
        pending_status(
            &self.codex.read().expect("Codex config poisoned"),
            resolve_codex_path,
        )
    }

    pub fn claude_code_status(&self) -> HarnessStatus {
        pending_status(
            &self
                .claude_code
                .read()
                .expect("Claude Code config poisoned"),
            resolve_claude_code_path,
        )
    }

    pub fn pi_status(&self) -> HarnessStatus {
        pending_status(
            &self.pi.read().expect("Pi config poisoned"),
            resolve_pi_path,
        )
    }

    pub fn copilot_status(&self) -> HarnessStatus {
        pending_status(
            &self.copilot.read().expect("Copilot config poisoned"),
            resolve_copilot_path,
        )
    }

    pub fn vscode_status(&self) -> HarnessStatus {
        pending_status(
            &self.vscode.read().expect("VS Code config poisoned"),
            resolve_vscode_path,
        )
    }

    pub fn connect_codex(
        &self,
        selected_model: String,
        proxy_endpoint: &str,
    ) -> Result<HarnessStatus, String> {
        let updated = config::set_codex_model(&self.fleet_config_dir, selected_model)?;
        let path = resolve_codex_path(&updated)?;
        sync_codex(
            &path,
            updated.selected_model.as_deref().unwrap(),
            proxy_endpoint,
        )?;
        *self.codex.write().expect("Codex config poisoned") = updated.clone();
        Ok(synced_status(path, updated.selected_model))
    }

    pub fn connect_claude_code(
        &self,
        selected_model: String,
        proxy_endpoint: &str,
    ) -> Result<HarnessStatus, String> {
        let updated = config::set_claude_code_model(&self.fleet_config_dir, selected_model)?;
        let path = resolve_claude_code_path(&updated)?;
        sync_claude_code(
            &path,
            updated.selected_model.as_deref().unwrap(),
            proxy_endpoint,
        )?;
        *self
            .claude_code
            .write()
            .expect("Claude Code config poisoned") = updated.clone();
        Ok(synced_status(path, updated.selected_model))
    }

    pub fn connect_pi(
        &self,
        selected_model: String,
        proxy_endpoint: &str,
        context_window: u32,
    ) -> Result<HarnessStatus, String> {
        let updated = config::set_pi_model(&self.fleet_config_dir, selected_model)?;
        let path = resolve_pi_path(&updated)?;
        sync_pi(
            &path,
            updated.selected_model.as_deref().unwrap(),
            proxy_endpoint,
            context_window,
        )?;
        *self.pi.write().expect("Pi config poisoned") = updated.clone();
        Ok(synced_status(path, updated.selected_model))
    }

    pub fn pi_agent_dir(&self) -> Result<PathBuf, String> {
        let path = resolve_pi_path(&self.pi.read().expect("Pi config poisoned"))?;
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("{} has no parent directory", path.display()))
    }

    pub fn connect_copilot(
        &self,
        selected_model: String,
        proxy_endpoint: &str,
    ) -> Result<HarnessStatus, String> {
        let updated = config::set_copilot_model(&self.fleet_config_dir, selected_model)?;
        let path = resolve_copilot_path(&updated)?;
        sync_copilot(
            &path,
            updated.selected_model.as_deref().unwrap(),
            proxy_endpoint,
        )?;
        *self.copilot.write().expect("Copilot config poisoned") = updated.clone();
        Ok(synced_status(path, updated.selected_model))
    }

    pub fn connect_vscode(
        &self,
        selected_model: String,
        proxy_endpoint: &str,
    ) -> Result<HarnessStatus, String> {
        let updated = config::set_vscode_model(&self.fleet_config_dir, selected_model)?;
        let path = resolve_vscode_path(&updated)?;
        sync_vscode(
            &path,
            updated.selected_model.as_deref().unwrap(),
            proxy_endpoint,
        )?;
        *self.vscode.write().expect("VS Code config poisoned") = updated.clone();
        Ok(synced_status(path, updated.selected_model))
    }
}

pub fn model_context_window(snapshot: &FleetSnapshot, selected_model: &str) -> u32 {
    selected_model
        .split_once('/')
        .and_then(|(host_id, model_id)| {
            snapshot
                .hosts
                .iter()
                .find(|host| host.id == host_id)
                .and_then(|host| host.models.iter().find(|model| model.id == model_id))
                .and_then(|model| model.context_length)
        })
        .unwrap_or(DEFAULT_CLIENT_CONTEXT_WINDOW)
}

pub type SharedLocalHarnessIntegrations = Arc<LocalHarnessIntegrations>;

fn pending_status(
    config: &HarnessConfig,
    resolver: fn(&HarnessConfig) -> Result<PathBuf, String>,
) -> HarnessStatus {
    if !config.enabled {
        return HarnessStatus::default();
    }
    match resolver(config) {
        Ok(path) => HarnessStatus {
            state: HarnessSyncState::Pending,
            config_path: Some(path.display().to_string()),
            selected_model: config.selected_model.clone(),
            ..HarnessStatus::default()
        },
        Err(error) => HarnessStatus {
            state: HarnessSyncState::Error,
            selected_model: config.selected_model.clone(),
            error: Some(error),
            ..HarnessStatus::default()
        },
    }
}

fn synced_status(path: PathBuf, selected_model: Option<String>) -> HarnessStatus {
    HarnessStatus {
        state: HarnessSyncState::Synced,
        config_path: Some(path.display().to_string()),
        selected_model,
        last_synced_at_ms: Some(now_ms()),
        error: None,
    }
}

fn resolve_codex_path(config: &HarnessConfig) -> Result<PathBuf, String> {
    if let Some(path) = explicit_path(config)? {
        return Ok(path);
    }
    if let Some(home) = env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join("config.toml"));
    }
    Ok(user_home()?.join(".codex").join("config.toml"))
}

fn resolve_claude_code_path(config: &HarnessConfig) -> Result<PathBuf, String> {
    if let Some(path) = explicit_path(config)? {
        return Ok(path);
    }
    Ok(user_home()?.join(".claude").join("settings.json"))
}

fn resolve_pi_path(config: &HarnessConfig) -> Result<PathBuf, String> {
    if let Some(path) = explicit_path(config)? {
        return Ok(path);
    }
    let directory = env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(user_home()?.join(".pi").join("agent"));
    Ok(directory.join("models.json"))
}

fn resolve_copilot_path(config: &HarnessConfig) -> Result<PathBuf, String> {
    if let Some(path) = explicit_path(config)? {
        return Ok(path);
    }
    let directory = env::var_os("COPILOT_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(user_home()?.join(".copilot"));
    Ok(directory.join("agentrelay.env"))
}

fn resolve_vscode_path(config: &HarnessConfig) -> Result<PathBuf, String> {
    if let Some(path) = explicit_path(config)? {
        return Ok(path);
    }
    #[cfg(windows)]
    {
        let app_data = env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| "cannot locate the Windows application-data directory".to_owned())?;
        Ok(app_data
            .join("Code")
            .join("User")
            .join("chatLanguageModels.json"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(user_home()?
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("chatLanguageModels.json"))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(user_home()?.join(".config"));
        Ok(config_home
            .join("Code")
            .join("User")
            .join("chatLanguageModels.json"))
    }
}

fn explicit_path(config: &HarnessConfig) -> Result<Option<PathBuf>, String> {
    let Some(value) = config
        .config_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        return Ok(Some(if value == "~" {
            user_home()?
        } else {
            user_home()?.join(&value[2..])
        }));
    }
    let path = PathBuf::from(value);
    Ok(Some(if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(|error| format!("cannot resolve harness config path: {error}"))?
            .join(path)
    }))
}

fn user_home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot locate the user home directory".to_owned())
}

fn sync_codex(path: &Path, selected_model: &str, proxy_endpoint: &str) -> Result<(), String> {
    let mut document = if path.exists() {
        fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?
            .parse::<DocumentMut>()
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
    } else {
        DocumentMut::new()
    };

    document["model"] = value(selected_model);
    document["model_provider"] = value(PROVIDER_ID);
    let providers = document
        .entry("model_providers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "model_providers must be a TOML table".to_owned())?;
    let provider = providers
        .entry(PROVIDER_ID)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("model_providers.{PROVIDER_ID} must be a TOML table"))?;
    provider["name"] = value("Agent Relay");
    provider["base_url"] = value(format!("{}/v1", proxy_endpoint.trim_end_matches('/')));
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(false);

    write_with_backup(path, document.to_string())
}

fn sync_claude_code(path: &Path, selected_model: &str, proxy_endpoint: &str) -> Result<(), String> {
    let mut root = if path.exists() {
        serde_json::from_str::<Value>(
            &fs::read_to_string(path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?,
        )
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
    } else {
        json!({})
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
    object.insert("model".into(), Value::String(selected_model.into()));
    let env = object_entry(object, "env", path)?;
    for (key, value) in [
        (
            "ANTHROPIC_BASE_URL",
            proxy_endpoint.trim_end_matches('/').to_owned(),
        ),
        ("ANTHROPIC_AUTH_TOKEN", "agentrelay-local".into()),
        ("ANTHROPIC_MODEL", selected_model.into()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", selected_model.into()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", selected_model.into()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", selected_model.into()),
        ("CLAUDE_CODE_SUBAGENT_MODEL", selected_model.into()),
        ("ANTHROPIC_CUSTOM_MODEL_OPTION", selected_model.into()),
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
            "Agent Relay model".into(),
        ),
        ("DISABLE_PROMPT_CACHING", "1".into()),
    ] {
        env.insert(key.into(), Value::String(value));
    }

    let contents = serde_json::to_string_pretty(&root)
        .map_err(|error| format!("failed to serialize Claude Code settings: {error}"))?;
    write_with_backup(path, format!("{contents}\n"))
}

fn sync_pi(
    path: &Path,
    selected_model: &str,
    proxy_endpoint: &str,
    context_window: u32,
) -> Result<(), String> {
    let mut models_root = read_json_object(path)?;
    let providers = object_entry(&mut models_root, "providers", path)?;
    providers.insert(
        PROVIDER_ID.into(),
        json!({
            "name": "Agent Relay",
            "baseUrl": format!("{}/v1", proxy_endpoint.trim_end_matches('/')),
            "api": "openai-completions",
            "apiKey": "agentrelay-local",
            "authHeader": false,
            "compat": {
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": false,
                "supportsUsageInStreaming": true,
                "maxTokensField": "max_tokens"
            },
            "models": [{
                "id": selected_model,
                "name": format!("{} · Agent Relay", selected_model),
                "reasoning": true,
                "input": ["text"],
                "contextWindow": context_window,
                "maxTokens": 16384,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
            }]
        }),
    );
    let models_contents = serde_json::to_string_pretty(&Value::Object(models_root))
        .map_err(|error| format!("failed to serialize Pi models: {error}"))?;
    write_with_backup(path, format!("{models_contents}\n"))?;

    let settings_path = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?
        .join("settings.json");
    let mut settings = read_json_object(&settings_path)?;
    settings.insert("defaultProvider".into(), Value::String(PROVIDER_ID.into()));
    settings.insert("defaultModel".into(), Value::String(selected_model.into()));
    let settings_contents = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|error| format!("failed to serialize Pi settings: {error}"))?;
    write_with_backup(&settings_path, format!("{settings_contents}\n"))
}

fn sync_copilot(path: &Path, selected_model: &str, proxy_endpoint: &str) -> Result<(), String> {
    let base_url = format!("{}/v1", proxy_endpoint.trim_end_matches('/'));
    let variables = [
        ("COPILOT_PROVIDER_BASE_URL", base_url.as_str()),
        ("COPILOT_PROVIDER_TYPE", "openai"),
        ("COPILOT_PROVIDER_API_KEY", "agentrelay-local"),
        ("COPILOT_MODEL", selected_model),
    ];
    write_with_backup(path, copilot_env_contents(&variables))?;
    persist_copilot_environment(path, &variables)
}

fn sync_vscode(path: &Path, selected_model: &str, proxy_endpoint: &str) -> Result<(), String> {
    let mut providers = if path.exists() {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        json5::from_str::<Value>(&contents)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
            .as_array()
            .cloned()
            .ok_or_else(|| format!("{} must contain a JSON array", path.display()))?
    } else {
        Vec::new()
    };
    let provider = json!({
        "name": "Agent Relay",
        "vendor": "customendpoint",
        "apiKey": "agentrelay-local",
        "apiType": "chat-completions",
        "models": [{
            "id": selected_model,
            "name": format!("{} · Agent Relay", selected_model),
            "url": format!("{}/v1/chat/completions", proxy_endpoint.trim_end_matches('/')),
            "toolCalling": true,
            "vision": false,
            "maxInputTokens": 49152,
            "maxOutputTokens": 16384,
            "streaming": true,
            "thinking": true
        }]
    });
    if let Some(existing) = providers.iter_mut().find(|candidate| {
        candidate.get("vendor").and_then(Value::as_str) == Some("customendpoint")
            && candidate.get("name").and_then(Value::as_str) == Some("Agent Relay")
    }) {
        *existing = provider;
    } else {
        providers.push(provider);
    }
    let contents = serde_json::to_string_pretty(&Value::Array(providers))
        .map_err(|error| format!("failed to serialize VS Code language models: {error}"))?;
    write_with_backup(path, format!("{contents}\n"))?;

    let settings_path = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?
        .join("settings.json");
    let mut settings = read_json5_object(&settings_path)?;
    settings.insert(
        "chat.agentHost.byokModels.enabled".into(),
        Value::Bool(true),
    );
    let settings_contents = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|error| format!("failed to serialize VS Code settings: {error}"))?;
    write_with_backup(&settings_path, format!("{settings_contents}\n"))
}

fn copilot_env_contents(variables: &[(&str, &str)]) -> String {
    let mut contents =
        String::from("# Managed by Agent Relay. Start a new Copilot CLI session after changes.\n");
    for (key, value) in variables {
        contents.push_str("export ");
        contents.push_str(key);
        contents.push('=');
        contents.push_str(&shell_quote(value));
        contents.push('\n');
    }
    contents
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn persist_copilot_environment(_path: &Path, variables: &[(&str, &str)]) -> Result<(), String> {
    for (key, value) in variables {
        let output = Command::new("setx")
            .args([key, value])
            .output()
            .map_err(|error| format!("failed to persist {key} for Copilot CLI: {error}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(format!("failed to persist {key} for Copilot CLI: {detail}"));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn persist_copilot_environment(path: &Path, variables: &[(&str, &str)]) -> Result<(), String> {
    let profile = user_home()?.join(".zshenv");
    let source = format!(
        "# >>> Agent Relay Copilot >>>\n[ -f {} ] && . {}\n# <<< Agent Relay Copilot <<<",
        shell_quote(path.to_string_lossy().as_ref()),
        shell_quote(path.to_string_lossy().as_ref())
    );
    let existing = if profile.exists() {
        fs::read_to_string(&profile)
            .map_err(|error| format!("failed to read {}: {error}", profile.display()))?
    } else {
        String::new()
    };
    let updated = upsert_managed_block(
        &existing,
        "# >>> Agent Relay Copilot >>>",
        "# <<< Agent Relay Copilot <<<",
        &source,
    );
    if updated != existing {
        write_with_backup(&profile, updated)?;
    }

    for (key, value) in variables {
        let output = Command::new("launchctl")
            .args(["setenv", key, value])
            .output()
            .map_err(|error| format!("failed to update launch environment for {key}: {error}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(format!(
                "failed to update launch environment for {key}: {detail}"
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn upsert_managed_block(existing: &str, start: &str, end: &str, block: &str) -> String {
    if let Some(start_index) = existing.find(start) {
        if let Some(relative_end) = existing[start_index..].find(end) {
            let end_index = start_index + relative_end + end.len();
            return format!(
                "{}{}{}",
                &existing[..start_index],
                block,
                &existing[end_index..]
            );
        }
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{existing}{separator}{block}\n")
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(
        &fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))
}

fn read_json5_object(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let value: Value = json5::from_str(
        &fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))
}

fn object_entry<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, String> {
    parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("{}.{} must be a JSON object", path.display(), key))
}

fn write_with_backup(path: &Path, contents: String) -> Result<(), String> {
    config::preserve_pristine_backup(path, ".agent-relay.bak")?;
    config::atomic_write_text(path, &contents)
}

#[cfg(test)]
fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "agent-relay-harness-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn codex_merge_preserves_user_settings_and_creates_provider() {
        let directory = test_directory("codex");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let original = "approval_policy = \"never\"\n";
        fs::write(&path, original).unwrap();

        sync_codex(&path, "workstation/qwen", "http://127.0.0.1:38475").unwrap();
        sync_codex(&path, "m1-pro/qwen", "http://127.0.0.1:38475").unwrap();

        let document = fs::read_to_string(&path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(document["approval_policy"].as_str(), Some("never"));
        assert_eq!(document["model"].as_str(), Some("m1-pro/qwen"));
        assert_eq!(
            document["model_providers"][PROVIDER_ID]["wire_api"].as_str(),
            Some("responses")
        );
        assert!(suffixed_path(&path, ".agent-relay.bak").exists());
        assert_eq!(
            fs::read_to_string(suffixed_path(&path, ".agent-relay.bak")).unwrap(),
            original
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn claude_merge_preserves_settings_and_pins_all_model_tiers() {
        let directory = test_directory("claude");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.json");
        fs::write(&path, r#"{"permissions":{"allow":["Read"]}}"#).unwrap();

        sync_claude_code(&path, "m1-pro/qwen", "http://127.0.0.1:38475").unwrap();

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["permissions"]["allow"][0], "Read");
        assert_eq!(value["model"], "m1-pro/qwen");
        assert_eq!(value["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "m1-pro/qwen");
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:38475");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pi_merge_adds_provider_and_selects_it_without_losing_settings() {
        let directory = test_directory("pi");
        fs::create_dir_all(&directory).unwrap();
        let models_path = directory.join("models.json");
        let settings_path = directory.join("settings.json");
        fs::write(&models_path, r#"{"providers":{"other":{"models":[]}}}"#).unwrap();
        fs::write(&settings_path, r#"{"theme":"dark"}"#).unwrap();

        sync_pi(
            &models_path,
            "air-m4/qwen",
            "http://127.0.0.1:38475",
            262_144,
        )
        .unwrap();

        let models: Value =
            serde_json::from_str(&fs::read_to_string(&models_path).unwrap()).unwrap();
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(models["providers"]["other"].is_object());
        assert_eq!(
            models["providers"][PROVIDER_ID]["models"][0]["id"],
            "air-m4/qwen"
        );
        assert_eq!(
            models["providers"][PROVIDER_ID]["models"][0]["contextWindow"],
            262_144
        );
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["defaultProvider"], PROVIDER_ID);
        assert_eq!(settings["defaultModel"], "air-m4/qwen");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copilot_manifest_contains_the_local_provider_and_qualified_model() {
        let variables = [
            ("COPILOT_PROVIDER_BASE_URL", "http://127.0.0.1:38475/v1"),
            ("COPILOT_PROVIDER_TYPE", "openai"),
            ("COPILOT_PROVIDER_API_KEY", "agentrelay-local"),
            ("COPILOT_MODEL", "workstation/qwen"),
        ];
        let contents = copilot_env_contents(&variables);
        assert!(contents.contains("COPILOT_PROVIDER_BASE_URL='http://127.0.0.1:38475/v1'"));
        assert!(contents.contains("COPILOT_MODEL='workstation/qwen'"));
    }

    #[test]
    fn vscode_merge_adds_custom_endpoint_and_enables_agent_host_byok() {
        let directory = test_directory("vscode");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("chatLanguageModels.json");
        fs::write(&path, r#"[{"name":"Other","vendor":"openai"}]"#).unwrap();
        fs::write(
            directory.join("settings.json"),
            "{// keep me\n\"theme\":\"dark\"}",
        )
        .unwrap();

        sync_vscode(&path, "workstation/qwen", "http://127.0.0.1:38475").unwrap();

        let providers: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(directory.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(providers[0]["name"], "Other");
        assert_eq!(providers[1]["vendor"], "customendpoint");
        assert_eq!(providers[1]["models"][0]["id"], "workstation/qwen");
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["chat.agentHost.byokModels.enabled"], true);
        fs::remove_dir_all(directory).unwrap();
    }
}
