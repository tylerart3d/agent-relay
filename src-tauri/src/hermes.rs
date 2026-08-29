use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::{
    channels::HarnessDeliveryResponse,
    config::{self, HermesConfig},
    domain::{HermesStatus, HermesSyncState},
    fleet_proxy::{client_proxy_base_url, ROUTED_MODEL_ID},
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DESKTOP_PLUGIN: &str = include_str!("../../integrations/hermes-desktop/plugin.js");

#[derive(Clone, Copy)]
enum HermesClientSurface {
    Desktop,
    Cli,
}

type DesktopPluginInstaller = fn() -> Result<(), String>;
type ConfigSetter = fn(&Path, &str, &str) -> Result<(), String>;

pub struct HermesIntegration {
    config: RwLock<HermesConfig>,
    fleet_config_dir: PathBuf,
    api_lock: tokio::sync::Mutex<()>,
}

impl HermesIntegration {
    pub fn new(config: HermesConfig, fleet_config_dir: PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            fleet_config_dir,
            api_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn prepare(&self, proxy_endpoint: &str) -> HermesStatus {
        let config = self
            .config
            .read()
            .expect("Hermes integration config poisoned")
            .clone();
        if !config.enabled {
            return HermesStatus {
                state: HermesSyncState::Disabled,
                ..HermesStatus::default()
            };
        }
        match install_desktop_plugin() {
            Ok(()) => match config
                .selected_model
                .as_deref()
                .and_then(|model| model.split_once('/'))
            {
                Some((host_id, model_id)) => self.sync_model_for(
                    host_id,
                    model_id,
                    proxy_endpoint,
                    HermesClientSurface::Desktop,
                    accept_desktop_plugin_install,
                    run_config_set,
                ),
                None => disabled_or_pending_status(
                    &config,
                    &self.fleet_config_dir,
                    config.selected_model.clone(),
                ),
            },
            Err(error) => HermesStatus {
                state: HermesSyncState::Error,
                executable_path: Some(
                    resolve_executable(&config, &self.fleet_config_dir)
                        .display()
                        .to_string(),
                ),
                error: Some(error),
                ..HermesStatus::default()
            },
        }
    }

    pub fn cli_status(&self) -> HermesStatus {
        let config = self
            .config
            .read()
            .expect("Hermes integration config poisoned")
            .clone();
        disabled_or_pending_status(
            &config,
            &self.fleet_config_dir,
            config.selected_cli_model.clone(),
        )
    }

    pub fn set_enabled(&self, enabled: bool, proxy_endpoint: &str) -> Result<HermesStatus, String> {
        let updated = config::set_hermes_enabled(&self.fleet_config_dir, enabled)?;
        *self
            .config
            .write()
            .expect("Hermes integration config poisoned") = updated.clone();
        Ok(if enabled {
            self.prepare(proxy_endpoint)
        } else {
            disabled_or_pending_status(
                &updated,
                &self.fleet_config_dir,
                updated.selected_model.clone(),
            )
        })
    }

    pub fn sync_model(&self, host_id: &str, model_id: &str, proxy_endpoint: &str) -> HermesStatus {
        self.sync_model_for(
            host_id,
            model_id,
            proxy_endpoint,
            HermesClientSurface::Desktop,
            install_desktop_plugin,
            run_config_set,
        )
    }

    /// Synchronizes the Hermes CLI configuration without installing or
    /// updating the optional Hermes Desktop bridge plugin.
    pub fn sync_cli_model(
        &self,
        host_id: &str,
        model_id: &str,
        proxy_endpoint: &str,
    ) -> HermesStatus {
        self.sync_model_for(
            host_id,
            model_id,
            proxy_endpoint,
            HermesClientSurface::Cli,
            install_desktop_plugin,
            run_config_set,
        )
    }

    fn sync_model_for(
        &self,
        host_id: &str,
        model_id: &str,
        proxy_endpoint: &str,
        surface: HermesClientSurface,
        install_plugin: DesktopPluginInstaller,
        set_config: ConfigSetter,
    ) -> HermesStatus {
        let config = self
            .config
            .read()
            .expect("Hermes integration config poisoned")
            .clone();
        if !config.enabled {
            return HermesStatus {
                state: HermesSyncState::Disabled,
                ..HermesStatus::default()
            };
        }

        if matches!(surface, HermesClientSurface::Desktop) {
            if let Err(error) = install_plugin() {
                return HermesStatus {
                    state: HermesSyncState::Error,
                    executable_path: Some(
                        resolve_executable(&config, &self.fleet_config_dir)
                            .display()
                            .to_string(),
                    ),
                    selected_model: Some(format!("{host_id}/{model_id}")),
                    error: Some(error),
                    ..HermesStatus::default()
                };
            }
        }

        let executable = resolve_executable(&config, &self.fleet_config_dir);
        let qualified_model = format!("{host_id}/{model_id}");
        let (configured_model, base_url) = match surface {
            HermesClientSurface::Desktop => (
                ROUTED_MODEL_ID,
                client_proxy_base_url(proxy_endpoint, "hermes"),
            ),
            HermesClientSurface::Cli => (
                qualified_model.as_str(),
                format!("{}/v1", proxy_endpoint.trim_end_matches('/')),
            ),
        };
        let context_window = config.context_window.to_string();
        for (key, value) in config_updates(configured_model, &base_url, &context_window) {
            if let Err(error) = set_config(&executable, key, value) {
                return HermesStatus {
                    state: HermesSyncState::Error,
                    executable_path: Some(executable.display().to_string()),
                    selected_model: Some(qualified_model.clone()),
                    last_synced_at_ms: None,
                    error: Some(error),
                };
            }
        }

        HermesStatus {
            state: HermesSyncState::Synced,
            executable_path: Some(executable.display().to_string()),
            selected_model: Some(qualified_model),
            last_synced_at_ms: Some(now_ms()),
            error: None,
        }
    }

    pub fn connect_model(
        &self,
        host_id: &str,
        model_id: &str,
        proxy_endpoint: &str,
    ) -> Result<HermesStatus, String> {
        self.persist_desktop_model(host_id, model_id)?;
        Ok(self.sync_model(host_id, model_id, proxy_endpoint))
    }

    /// Persists a CLI selection and applies only Hermes' supported config
    /// updates. Desktop plugin installation is deliberately out of this path.
    pub fn connect_cli_model(
        &self,
        host_id: &str,
        model_id: &str,
        proxy_endpoint: &str,
    ) -> Result<HermesStatus, String> {
        self.persist_cli_model(host_id, model_id)?;
        Ok(self.sync_cli_model(host_id, model_id, proxy_endpoint))
    }

    fn persist_desktop_model(&self, host_id: &str, model_id: &str) -> Result<(), String> {
        let qualified_model = format!("{host_id}/{model_id}");
        let updated = config::set_hermes_model(&self.fleet_config_dir, qualified_model)?;
        *self
            .config
            .write()
            .expect("Hermes integration config poisoned") = updated;
        Ok(())
    }

    fn persist_cli_model(&self, host_id: &str, model_id: &str) -> Result<(), String> {
        let qualified_model = format!("{host_id}/{model_id}");
        let updated = config::set_hermes_cli_model(&self.fleet_config_dir, qualified_model)?;
        *self
            .config
            .write()
            .expect("Hermes integration config poisoned") = updated;
        Ok(())
    }

    pub fn set_context_window(&self, context_window: u32) -> Result<HermesStatus, String> {
        let updated = config::set_hermes_context_window(&self.fleet_config_dir, context_window)?;
        let executable = resolve_executable(&updated, &self.fleet_config_dir);
        *self
            .config
            .write()
            .expect("Hermes integration config poisoned") = updated.clone();
        run_config_set(
            &executable,
            "model.context_length",
            &context_window.to_string(),
        )?;
        Ok(HermesStatus {
            state: HermesSyncState::Synced,
            executable_path: Some(executable.display().to_string()),
            selected_model: updated.selected_model,
            last_synced_at_ms: Some(now_ms()),
            error: None,
        })
    }

    pub fn context_window(&self) -> u32 {
        self.config
            .read()
            .expect("Hermes integration config poisoned")
            .context_window
    }

    pub fn executable_path(&self) -> PathBuf {
        let config = self
            .config
            .read()
            .expect("Hermes integration config poisoned");
        resolve_executable(&config, &self.fleet_config_dir)
    }

    pub async fn deliver_api_message(
        &self,
        host_id: &str,
        model_id: &str,
        proxy_endpoint: &str,
        native_session_id: &str,
        idempotency_key: &str,
        text: &str,
    ) -> Result<HarnessDeliveryResponse, String> {
        let _guard = self.api_lock.lock().await;
        let status = self.sync_cli_model(host_id, model_id, proxy_endpoint);
        if status.state != HermesSyncState::Synced {
            return Err(status
                .error
                .unwrap_or_else(|| "Hermes model configuration did not synchronize".into()));
        }

        let key = self.api_server_key()?;
        let conversation = native_session_id.to_owned();
        let qualified_model = format!("{host_id}/{model_id}");
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .map_err(|error| format!("failed to create Hermes API client: {error}"))?;
        self.ensure_api_server(&client, &key).await?;
        let response = client
            .post("http://127.0.0.1:8642/v1/responses")
            .bearer_auth(&key)
            .header("Idempotency-Key", idempotency_key)
            .json(&serde_json::json!({
                "provider": "custom:local",
                "model": qualified_model,
                "input": text,
                "conversation": &conversation,
                "store": true,
            }))
            .send()
            .await
            .map_err(|error| format!("failed to contact Hermes API server: {error}"))?;
        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| format!("Hermes API returned invalid JSON: {error}"))?;
        if !status.is_success() {
            return Err(payload
                .pointer("/error/message")
                .or_else(|| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Hermes API request failed")
                .to_owned());
        }
        let reply = hermes_response_text(&payload)
            .ok_or_else(|| "Hermes API returned no assistant text".to_owned())?;
        Ok(HarnessDeliveryResponse {
            reply,
            native_session_id: Some(conversation),
        })
    }

    async fn ensure_api_server(&self, client: &reqwest::Client, key: &str) -> Result<(), String> {
        if hermes_api_healthy(client, key).await {
            return Ok(());
        }
        let executable = self.executable_path();
        let mut command = Command::new(&executable);
        command
            .args(["gateway", "run"])
            .env("API_SERVER_ENABLED", "true")
            .env("API_SERVER_HOST", "127.0.0.1")
            .env("API_SERVER_PORT", "8642")
            .env("API_SERVER_KEY", key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        command.spawn().map_err(|error| {
            format!(
                "failed to start Hermes API server with {}: {error}",
                executable.display()
            )
        })?;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if hermes_api_healthy(client, key).await {
                return Ok(());
            }
        }
        Err("Hermes API server did not become ready within 20 seconds".into())
    }

    fn api_server_key(&self) -> Result<String, String> {
        if let Some(key) = read_dotenv_value(&hermes_home().join(".env"), "API_SERVER_KEY")? {
            return Ok(key);
        }
        let path = self.fleet_config_dir.join("hermes-api.key");
        if path.exists() {
            let key = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?
                .trim()
                .to_owned();
            if !key.is_empty() {
                return Ok(key);
            }
        }
        let key = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        crate::config::atomic_write_text(&path, &key)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|error| {
                format!(
                    "failed to protect Hermes API key {}: {error}",
                    path.display()
                )
            })?;
        }
        Ok(key)
    }
}

async fn hermes_api_healthy(client: &reqwest::Client, key: &str) -> bool {
    client
        .get("http://127.0.0.1:8642/health")
        .bearer_auth(key)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn read_dotenv_value(path: &Path, key: &str) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    Ok(contents.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches(['\'', '"']).to_owned())
    }))
}

fn hermes_response_text(payload: &serde_json::Value) -> Option<String> {
    let text = payload
        .get("output")?
        .as_array()?
        .iter()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("message")
                && item.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
        })
        .filter_map(|item| item.get("content").and_then(serde_json::Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(serde_json::Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

pub type SharedHermesIntegration = Arc<HermesIntegration>;

fn disabled_or_pending_status(
    config: &HermesConfig,
    fleet_config_dir: &Path,
    selected_model: Option<String>,
) -> HermesStatus {
    HermesStatus {
        state: if config.enabled {
            HermesSyncState::Pending
        } else {
            HermesSyncState::Disabled
        },
        executable_path: config.enabled.then(|| {
            resolve_executable(config, fleet_config_dir)
                .display()
                .to_string()
        }),
        selected_model,
        ..HermesStatus::default()
    }
}

fn config_updates<'a>(
    model: &'a str,
    base_url: &'a str,
    context_window: &'a str,
) -> [(&'static str, &'a str); 8] {
    [
        ("providers.local.name", "Agent Relay"),
        ("providers.local.api", base_url),
        ("providers.local.transport", "openai_chat"),
        ("providers.local.request_timeout_seconds", "900"),
        ("model.provider", "custom:local"),
        ("model.base_url", base_url),
        ("model.default", model),
        ("model.context_length", context_window),
    ]
}

fn run_config_set(executable: &Path, key: &str, value: &str) -> Result<(), String> {
    let mut command = Command::new(executable);
    command.args(["config", "set", key, value]);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let output = command.output().map_err(|error| {
        format!(
            "failed to run {} config set {key}: {error}",
            executable.display()
        )
    })?;
    if output.status.success() {
        return Ok(());
    }

    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!(
        "{} config set {key} failed ({}): {}",
        executable.display(),
        output.status,
        if detail.is_empty() {
            "no error detail"
        } else {
            &detail
        }
    ))
}

fn install_desktop_plugin() -> Result<(), String> {
    let home = hermes_home();
    let mut roots = vec![home.clone()];
    let profiles = home.join("profiles");
    if let Ok(entries) = fs::read_dir(&profiles) {
        roots.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir()),
        );
    }

    for root in roots {
        let directory = root.join("desktop-plugins").join("agent-relay");
        fs::create_dir_all(&directory).map_err(|error| {
            format!(
                "failed to create Hermes desktop plugin directory {}: {error}",
                directory.display()
            )
        })?;
        let path = directory.join("plugin.js");
        if fs::read_to_string(&path).ok().as_deref() != Some(DESKTOP_PLUGIN) {
            config::preserve_pristine_backup(&path, ".agent-relay.bak")?;
            config::atomic_write_text(&path, DESKTOP_PLUGIN).map_err(|error| {
                format!(
                    "failed to install Hermes desktop plugin {}: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn accept_desktop_plugin_install() -> Result<(), String> {
    Ok(())
}

fn hermes_home() -> PathBuf {
    if let Some(value) = env::var_os("HERMES_HOME") {
        return PathBuf::from(value);
    }
    #[cfg(windows)]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let current = PathBuf::from(local_app_data).join("hermes");
        let legacy = env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|home| home.join(".hermes"));
        if current.is_dir() || legacy.as_ref().is_none_or(|path| !path.is_dir()) {
            return current;
        }
        return legacy.expect("legacy Hermes home was checked");
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hermes")
}

fn resolve_executable(config: &HermesConfig, fleet_config_dir: &Path) -> PathBuf {
    if let Some(value) = config
        .executable_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return resolve_explicit_path(value, fleet_config_dir);
    }

    for candidate in platform_candidates() {
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(if cfg!(windows) {
        "hermes.exe"
    } else {
        "hermes"
    })
}

fn platform_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(windows)]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_app_data)
                .join("hermes")
                .join("hermes-agent")
                .join("venv")
                .join("Scripts")
                .join("hermes.exe"),
        );
    }
    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        candidates.push(home.join(".local").join("bin").join(if cfg!(windows) {
            "hermes.exe"
        } else {
            "hermes"
        }));
    }
    candidates
}

fn resolve_explicit_path(value: &str, fleet_config_dir: &Path) -> PathBuf {
    let expanded = if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .map(|home| {
                if value == "~" {
                    home
                } else {
                    home.join(&value[2..])
                }
            })
            .unwrap_or_else(|| PathBuf::from(value))
    } else {
        PathBuf::from(value)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        fleet_config_dir.join(expanded)
    }
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

    #[test]
    fn extracts_responses_api_assistant_text() {
        let payload = serde_json::json!({
            "output": [
                {"type": "function_call", "name": "terminal"},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "done "},
                    {"type": "refusal", "text": "ignored"},
                    {"type": "output_text", "text": "well"}
                ]}
            ]
        });
        assert_eq!(hermes_response_text(&payload).as_deref(), Some("done well"));
        assert!(hermes_response_text(&serde_json::json!({"output": []})).is_none());
    }

    #[test]
    fn reads_quoted_api_key_from_hermes_dotenv() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-hermes-dotenv-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&directory).expect("create dotenv test directory");
        let path = directory.join(".env");
        fs::write(&path, "# comment\nAPI_SERVER_KEY='local secret'\n").expect("write dotenv");
        assert_eq!(
            read_dotenv_value(&path, "API_SERVER_KEY")
                .unwrap()
                .as_deref(),
            Some("local secret")
        );
        fs::remove_dir_all(directory).expect("remove dotenv test directory");
    }

    fn reject_desktop_plugin_install() -> Result<(), String> {
        Err("desktop plugin directory is unavailable".into())
    }

    fn accept_config_update(_executable: &Path, _key: &str, _value: &str) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn desktop_uses_one_virtual_model_on_its_client_route() {
        let base_url = client_proxy_base_url("http://127.0.0.1:38475", "hermes");
        assert_eq!(
            config_updates(ROUTED_MODEL_ID, &base_url, "65536"),
            [
                ("providers.local.name", "Agent Relay"),
                (
                    "providers.local.api",
                    "http://127.0.0.1:38475/clients/hermes/v1"
                ),
                ("providers.local.transport", "openai_chat"),
                ("providers.local.request_timeout_seconds", "900"),
                ("model.provider", "custom:local"),
                ("model.base_url", "http://127.0.0.1:38475/clients/hermes/v1"),
                ("model.default", "agentrelay"),
                ("model.context_length", "65536"),
            ]
        );
    }

    #[test]
    fn disabled_integration_has_no_selected_model() {
        let integration = HermesIntegration::new(
            HermesConfig {
                enabled: false,
                executable_path: None,
                selected_model: None,
                selected_cli_model: None,
                context_window: 65_536,
            },
            PathBuf::from("config"),
        );
        let status = integration.sync_model("workstation", "qwen", "http://localhost:38475");
        assert_eq!(status.state, HermesSyncState::Disabled);
        assert_eq!(status.selected_model, None);
    }

    #[test]
    fn relative_executable_paths_resolve_beside_fleet_config() {
        let config = HermesConfig {
            enabled: true,
            executable_path: Some("bin/hermes".into()),
            selected_model: None,
            selected_cli_model: None,
            context_window: 65_536,
        };
        assert_eq!(
            resolve_executable(&config, Path::new("fleet-config")),
            Path::new("fleet-config").join("bin/hermes")
        );
    }

    #[test]
    fn cli_status_uses_its_own_persisted_selection() {
        let integration = HermesIntegration::new(
            HermesConfig {
                enabled: true,
                executable_path: Some("bin/hermes".into()),
                selected_model: Some("workstation/desktop".into()),
                selected_cli_model: Some("m1-pro/cli".into()),
                context_window: 65_536,
            },
            PathBuf::from("config"),
        );
        assert_eq!(
            integration.cli_status().selected_model.as_deref(),
            Some("m1-pro/cli")
        );
    }

    #[test]
    fn cli_sync_does_not_depend_on_the_desktop_plugin_installer() {
        let integration = HermesIntegration::new(
            HermesConfig {
                enabled: true,
                executable_path: Some("bin/hermes".into()),
                selected_model: None,
                selected_cli_model: None,
                context_window: 65_536,
            },
            PathBuf::from("config"),
        );

        let cli = integration.sync_model_for(
            "workstation",
            "qwen",
            "http://localhost:38475",
            HermesClientSurface::Cli,
            reject_desktop_plugin_install,
            accept_config_update,
        );
        assert_eq!(cli.state, HermesSyncState::Synced);
        assert_eq!(cli.selected_model.as_deref(), Some("workstation/qwen"));

        let desktop = integration.sync_model_for(
            "workstation",
            "qwen",
            "http://localhost:38475",
            HermesClientSurface::Desktop,
            reject_desktop_plugin_install,
            accept_config_update,
        );
        assert_eq!(desktop.state, HermesSyncState::Error);
        assert_eq!(
            desktop.error.as_deref(),
            Some("desktop plugin directory is unavailable")
        );
    }
}
