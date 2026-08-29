use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    channels::{HarnessDeliveryRequest, HarnessDeliveryResponse},
    config::{self, OpenCodeConfig},
    domain::{FleetSnapshot, OpenCodeStatus, OpenCodeSyncState},
    fleet::SharedFleetService,
    fleet_proxy::{client_proxy_base_url, ROUTED_MODEL_ID},
    terminal::{self, CliHarness},
};

const PROVIDER_ID: &str = "agentrelay";
const API_ENDPOINT: &str = "http://127.0.0.1:38476";
const API_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const TURN_TIMEOUT: Duration = Duration::from_secs(25 * 60);
const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const API_POLL_INTERVAL: Duration = Duration::from_secs(1);
const API_START_GRACE: Duration = Duration::from_secs(5);
const API_CONTINUATION_GRACE: Duration = Duration::from_secs(10);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OpenCodeSessionInfo {
    pub id: String,
    pub title: String,
    pub project_id: String,
    pub project_name: String,
    pub directory: String,
    pub updated_at_ms: u64,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_model: Option<String>,
}

pub struct OpenCodeIntegration {
    config: RwLock<OpenCodeConfig>,
    fleet_config_dir: PathBuf,
    api_lock: tokio::sync::Mutex<()>,
}

impl OpenCodeIntegration {
    pub fn new(config: OpenCodeConfig, fleet_config_dir: PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            fleet_config_dir,
            api_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn sync(&self, snapshot: &FleetSnapshot) -> OpenCodeStatus {
        let config = self
            .config
            .read()
            .expect("OpenCode integration config poisoned")
            .clone();
        sync_config(&config, &self.fleet_config_dir, snapshot)
    }

    pub fn set_enabled(
        &self,
        enabled: bool,
        snapshot: &FleetSnapshot,
    ) -> Result<OpenCodeStatus, String> {
        let updated = config::set_opencode_enabled(&self.fleet_config_dir, enabled)?;
        *self
            .config
            .write()
            .expect("OpenCode integration config poisoned") = updated;
        Ok(self.sync(snapshot))
    }

    pub fn connect_model(
        &self,
        selected_model: String,
        snapshot: &FleetSnapshot,
    ) -> Result<OpenCodeStatus, String> {
        let updated = config::set_opencode_model(&self.fleet_config_dir, selected_model)?;
        *self
            .config
            .write()
            .expect("OpenCode integration config poisoned") = updated;
        Ok(self.sync(snapshot))
    }

    pub fn set_context_window(
        &self,
        context_window: u32,
        snapshot: &FleetSnapshot,
    ) -> Result<OpenCodeStatus, String> {
        let updated = config::set_opencode_context_window(&self.fleet_config_dir, context_window)?;
        *self
            .config
            .write()
            .expect("OpenCode integration config poisoned") = updated;
        Ok(self.sync(snapshot))
    }

    pub fn context_window(&self) -> u32 {
        self.config
            .read()
            .expect("OpenCode integration config poisoned")
            .context_window
    }

    pub async fn deliver_api_message(
        &self,
        request: &HarnessDeliveryRequest,
        fleet: &SharedFleetService,
    ) -> Result<HarnessDeliveryResponse, String> {
        let _guard = tokio::time::timeout(API_LOCK_TIMEOUT, self.api_lock.lock())
            .await
            .map_err(|_| {
                "OpenCode is still finishing a previous Agent Relay message; try again shortly"
                    .to_owned()
            })?;
        let snapshot = fleet.snapshot();
        let selected_model = format!("{}/{}", request.host_id, request.model_id);
        let status = self.connect_model(selected_model, &snapshot)?;
        if status.state != OpenCodeSyncState::Synced {
            return Err(status
                .error
                .unwrap_or_else(|| "OpenCode model configuration did not synchronize".into()));
        }
        fleet.update_opencode_status(status);

        let project = resolve_project_directory(request.project.as_deref())?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(API_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| format!("failed to create OpenCode API client: {error}"))?;
        self.ensure_api_server(&client, &project).await?;
        let native_session_id = match request.native_session_id.as_deref() {
            Some(session_id) => session_id.to_owned(),
            None => create_api_session(&client, &project, request.session_id).await?,
        };
        if api_session_busy(&client, &project, &native_session_id).await? {
            return Err(
                "OpenCode is already working in this session; wait for it to finish or stop the active turn before sending another message"
                    .into(),
            );
        }
        let baseline = api_message_ids(&client, &project, &native_session_id).await?;
        let response = client
            .post(api_url(
                &format!("session/{native_session_id}/prompt_async"),
                &project,
            )?)
            .json(&json!({
                "model": { "providerID": PROVIDER_ID, "modelID": ROUTED_MODEL_ID },
                "parts": [{ "type": "text", "text": request.text }]
            }))
            .send()
            .await
            .map_err(|error| format!("failed to submit the OpenCode message: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let payload: Value = response.json().await.unwrap_or(Value::Null);
            return Err(api_error(&payload)
                .unwrap_or_else(|| format!("OpenCode API returned HTTP {status}")));
        }
        let reply = match wait_for_api_reply(&client, &project, &native_session_id, &baseline).await
        {
            Ok(reply) => reply,
            Err(error) => {
                abort_api_session(&client, &project, &native_session_id).await;
                return Err(error);
            }
        };
        Ok(HarnessDeliveryResponse {
            reply,
            native_session_id: Some(native_session_id),
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<OpenCodeSessionInfo>, String> {
        let mut sessions = read_session_inventory(&opencode_database_path()?)?;
        let selected_model = self
            .config
            .read()
            .expect("OpenCode integration config poisoned")
            .selected_model
            .clone();
        apply_relay_model(&mut sessions, selected_model);
        Ok(sessions)
    }

    pub fn set_session_archived(
        &self,
        native_session_id: &str,
        archived: bool,
    ) -> Result<(), String> {
        set_opencode_session_archived(&opencode_database_path()?, native_session_id, archived)
    }

    async fn ensure_api_server(
        &self,
        client: &reqwest::Client,
        project: &Path,
    ) -> Result<(), String> {
        if opencode_api_healthy(client).await {
            return Ok(());
        }
        let executable = terminal::resolve_executable(CliHarness::OpenCode)?;
        let config = self
            .config
            .read()
            .expect("OpenCode integration config poisoned")
            .clone();
        let config_path = resolve_config_path(&config, &self.fleet_config_dir)?;
        spawn_api_server(&executable, &config_path, project)?;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if opencode_api_healthy(client).await {
                return Ok(());
            }
        }
        Err("OpenCode API server did not become ready within 20 seconds".into())
    }
}

async fn abort_api_session(client: &reqwest::Client, project: &Path, session_id: &str) {
    let Ok(url) = api_url(&format!("session/{session_id}/abort"), project) else {
        return;
    };
    let _ = client
        .post(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
}

async fn api_messages(
    client: &reqwest::Client,
    project: &Path,
    session_id: &str,
) -> Result<Vec<Value>, String> {
    let response = client
        .get(api_url(&format!("session/{session_id}/message"), project)?)
        .send()
        .await
        .map_err(|error| format!("failed to inspect the OpenCode session: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "OpenCode session inspection returned HTTP {}",
            response.status()
        ));
    }
    response
        .json()
        .await
        .map_err(|error| format!("OpenCode session inspection returned invalid JSON: {error}"))
}

async fn api_message_ids(
    client: &reqwest::Client,
    project: &Path,
    session_id: &str,
) -> Result<HashSet<String>, String> {
    Ok(api_messages(client, project, session_id)
        .await?
        .iter()
        .filter_map(api_message_id)
        .map(str::to_owned)
        .collect())
}

async fn api_session_busy(
    client: &reqwest::Client,
    project: &Path,
    session_id: &str,
) -> Result<bool, String> {
    let response = client
        .get(api_url("session/status", project)?)
        .send()
        .await
        .map_err(|error| format!("failed to inspect OpenCode session status: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "OpenCode session status returned HTTP {}",
            response.status()
        ));
    }
    let statuses: Value = response
        .json()
        .await
        .map_err(|error| format!("OpenCode session status returned invalid JSON: {error}"))?;
    Ok(statuses.get(session_id).is_some())
}

async fn wait_for_api_reply(
    client: &reqwest::Client,
    project: &Path,
    session_id: &str,
    baseline: &HashSet<String>,
) -> Result<String, String> {
    let started = tokio::time::Instant::now();
    let deadline = started + TURN_TIMEOUT;
    let mut continuation_candidate: Option<(String, tokio::time::Instant)> = None;
    loop {
        let messages = api_messages(client, project, session_id).await?;
        let new_assistants: Vec<&Value> = messages
            .iter()
            .filter(|message| {
                message.pointer("/info/role").and_then(Value::as_str) == Some("assistant")
                    && api_message_id(message).is_some_and(|id| !baseline.contains(id))
            })
            .collect();
        let busy = api_session_busy(client, project, session_id).await?;
        if !busy && !new_assistants.is_empty() {
            match completed_api_reply(&new_assistants) {
                Ok(reply) => return Ok(reply),
                Err(error) if api_reply_may_continue(&new_assistants) => {
                    let candidate_id = new_assistants
                        .last()
                        .and_then(|message| api_message_id(message))
                        .unwrap_or_default()
                        .to_owned();
                    match continuation_candidate.as_ref() {
                        Some((previous_id, since))
                            if previous_id == &candidate_id
                                && since.elapsed() >= API_CONTINUATION_GRACE =>
                        {
                            return Err(error);
                        }
                        Some((previous_id, _)) if previous_id == &candidate_id => {}
                        _ => {
                            continuation_candidate =
                                Some((candidate_id, tokio::time::Instant::now()));
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        } else if busy {
            continuation_candidate = None;
        }
        if !busy && new_assistants.is_empty() && started.elapsed() >= API_START_GRACE {
            return Err("OpenCode accepted the message but did not start an assistant turn".into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("OpenCode exceeded 25 minutes, so Agent Relay cancelled the turn".into());
        }
        tokio::time::sleep(API_POLL_INTERVAL).await;
    }
}

fn api_reply_may_continue(messages: &[&Value]) -> bool {
    let Some(message) = messages.last() else {
        return false;
    };
    if opencode_response_text(message).is_some() {
        return false;
    }
    match message.pointer("/info/error") {
        None => true,
        Some(error) => {
            error.get("name").and_then(Value::as_str) == Some("MessageAbortedError")
                || api_error(error).as_deref() == Some("Aborted")
        }
    }
}

fn api_message_id(message: &Value) -> Option<&str> {
    message.pointer("/info/id").and_then(Value::as_str)
}

fn completed_api_reply(messages: &[&Value]) -> Result<String, String> {
    for message in messages.iter().rev() {
        if let Some(error) = message.pointer("/info/error") {
            return Err(api_error(error).unwrap_or_else(|| "OpenCode agent failed".into()));
        }
        if let Some(reply) = opencode_response_text(message) {
            return Ok(reply);
        }
    }
    Err("OpenCode completed the turn without assistant text".into())
}

fn apply_relay_model(sessions: &mut [OpenCodeSessionInfo], selected_model: Option<String>) {
    for session in sessions {
        if session.provider_id.as_deref() == Some(PROVIDER_ID)
            && session.model_id.as_deref() == Some(ROUTED_MODEL_ID)
        {
            session.relay_model = selected_model.clone();
        }
    }
}

fn opencode_database_path() -> Result<PathBuf, String> {
    if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(data_home)
            .join("opencode")
            .join("opencode.db"));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot locate the user home directory".to_owned())?;
    Ok(home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db"))
}

fn read_session_inventory(path: &Path) -> Result<Vec<OpenCodeSessionInfo>, String> {
    if !path.is_file() {
        return Err(format!(
            "OpenCode session database was not found at {}",
            path.display()
        ));
    }
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to open OpenCode session database: {error}"))?;
    let mut statement = connection
        .prepare(
            "SELECT s.id, s.title, s.project_id, COALESCE(p.name, ''), s.directory, \
                    s.time_updated, s.time_archived, s.model \
             FROM session s \
             LEFT JOIN project p ON p.id = s.project_id \
             WHERE s.parent_id IS NULL \
             ORDER BY s.time_updated DESC \
             LIMIT 200",
        )
        .map_err(|error| format!("failed to inspect OpenCode sessions: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let updated_at_ms: i64 = row.get(5)?;
            let archived_at_ms: Option<i64> = row.get(6)?;
            let model: Option<String> = row.get(7)?;
            let model = model
                .as_deref()
                .and_then(|value| serde_json::from_str::<Value>(value).ok());
            Ok(OpenCodeSessionInfo {
                id: row.get(0)?,
                title: row.get(1)?,
                project_id: row.get(2)?,
                project_name: row.get(3)?,
                directory: row.get(4)?,
                updated_at_ms: updated_at_ms.max(0) as u64,
                archived: archived_at_ms.is_some(),
                provider_id: model
                    .as_ref()
                    .and_then(|value| value.get("providerID"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                model_id: model
                    .as_ref()
                    .and_then(|value| value.get("id").or_else(|| value.get("modelID")))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                relay_model: None,
            })
        })
        .map_err(|error| format!("failed to read OpenCode sessions: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to decode OpenCode sessions: {error}"))
}

fn set_opencode_session_archived(
    path: &Path,
    native_session_id: &str,
    archived: bool,
) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!(
            "OpenCode session database was not found at {}",
            path.display()
        ));
    }
    let connection = rusqlite::Connection::open(path)
        .map_err(|error| format!("failed to open OpenCode session database: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("failed to configure OpenCode session database: {error}"))?;
    let archived_at = archived.then(|| now_ms().min(i64::MAX as u64) as i64);
    let changed = connection
        .execute(
            "UPDATE session SET time_archived = ?1 WHERE id = ?2",
            rusqlite::params![archived_at, native_session_id],
        )
        .map_err(|error| {
            format!("failed to update OpenCode session {native_session_id}: {error}")
        })?;
    if changed == 0 {
        return Err(format!(
            "OpenCode session {native_session_id} was not found"
        ));
    }
    Ok(())
}

async fn opencode_api_healthy(client: &reqwest::Client) -> bool {
    client
        .get(format!("{API_ENDPOINT}/global/health"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

async fn create_api_session(
    client: &reqwest::Client,
    project: &Path,
    session_id: u64,
) -> Result<String, String> {
    let response = client
        .post(api_url("session", project)?)
        .json(&json!({ "title": format!("Agent Relay session #{session_id}") }))
        .send()
        .await
        .map_err(|error| format!("failed to create OpenCode session: {error}"))?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|error| format!("OpenCode session creation returned invalid JSON: {error}"))?;
    if !status.is_success() {
        return Err(api_error(&payload)
            .unwrap_or_else(|| format!("OpenCode session creation returned HTTP {status}")));
    }
    payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "OpenCode session creation returned no session ID".into())
}

fn api_url(path: &str, project: &Path) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(&format!("{API_ENDPOINT}/{path}"))
        .map_err(|error| format!("failed to construct OpenCode API URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("directory", project.to_string_lossy().as_ref());
    Ok(url)
}

fn resolve_project_directory(project: Option<&str>) -> Result<PathBuf, String> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot locate the user home directory".to_owned())?;
    let path = match project.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            }
        }
        None => home,
    };
    if !path.is_dir() {
        return Err(format!(
            "OpenCode project directory {} does not exist; use an absolute path when the project is not under the harness user's home directory",
            path.display()
        ));
    }
    Ok(path)
}

fn opencode_response_text(payload: &Value) -> Option<String> {
    let text = payload
        .get("parts")?
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

fn api_error(payload: &Value) -> Option<String> {
    payload
        .pointer("/data/message")
        .or_else(|| payload.pointer("/error/message"))
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(not(windows))]
fn spawn_api_server(executable: &Path, config_path: &Path, project: &Path) -> Result<(), String> {
    Command::new(executable)
        .args(["serve", "--hostname", "127.0.0.1", "--port", "38476"])
        .env("OPENCODE_CONFIG", config_path)
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start OpenCode API server: {error}"))
}

#[cfg(windows)]
fn spawn_api_server(executable: &Path, config_path: &Path, project: &Path) -> Result<(), String> {
    fn literal(value: &Path) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "''"))
    }
    let script = format!(
        "$env:OPENCODE_CONFIG={}; Set-Location -LiteralPath {}; & {} serve --hostname 127.0.0.1 --port 38476",
        literal(config_path),
        literal(project),
        literal(executable)
    );
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start OpenCode API server: {error}"))
}

pub type SharedOpenCodeIntegration = Arc<OpenCodeIntegration>;

fn sync_config(
    config: &OpenCodeConfig,
    fleet_config_dir: &Path,
    snapshot: &FleetSnapshot,
) -> OpenCodeStatus {
    if !config.enabled {
        return OpenCodeStatus::default();
    }

    let path = match resolve_config_path(config, fleet_config_dir) {
        Ok(path) => path,
        Err(error) => return error_status(None, config.selected_model.clone(), error),
    };
    let model_count = 1;

    match sync_path(
        &path,
        snapshot,
        config.selected_model.as_deref(),
        config.context_window,
    ) {
        Ok(()) => OpenCodeStatus {
            state: OpenCodeSyncState::Synced,
            config_path: Some(path.display().to_string()),
            model_count,
            selected_model: config.selected_model.clone(),
            last_synced_at_ms: Some(now_ms()),
            error: None,
        },
        Err(error) => error_status(Some(path), config.selected_model.clone(), error),
    }
}

fn resolve_config_path(
    config: &OpenCodeConfig,
    fleet_config_dir: &Path,
) -> Result<PathBuf, String> {
    if let Some(config_path) = config
        .config_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        return resolve_explicit_path(config_path, fleet_config_dir);
    }

    if let Some(config_path) = env::var_os("OPENCODE_CONFIG").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(config_path));
    }

    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| "cannot locate the user home directory".to_owned())?;
    let directory = PathBuf::from(home).join(".config").join("opencode");
    let json = directory.join("opencode.json");
    let jsonc = directory.join("opencode.jsonc");
    if json.exists() && jsonc.exists() {
        return Err(format!(
            "both {} and {} exist; set opencode.config_path explicitly",
            json.display(),
            jsonc.display()
        ));
    }
    Ok(if jsonc.exists() { jsonc } else { json })
}

fn resolve_explicit_path(value: &str, fleet_config_dir: &Path) -> Result<PathBuf, String> {
    let expanded = if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .ok_or_else(|| "cannot expand '~' without a user home directory".to_owned())?;
        if value == "~" {
            PathBuf::from(home)
        } else {
            PathBuf::from(home).join(&value[2..])
        }
    } else {
        PathBuf::from(value)
    };

    Ok(if expanded.is_absolute() {
        expanded
    } else {
        fleet_config_dir.join(expanded)
    })
}

fn sync_path(
    path: &Path,
    snapshot: &FleetSnapshot,
    _selected_model: Option<&str>,
    context_window: u32,
) -> Result<(), String> {
    let mut root = if path.exists() {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        json5::from_str::<Value>(&contents)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
    } else {
        json!({ "$schema": "https://opencode.ai/config.json" })
    };

    let root_object = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
    let mut changed = false;
    let desired = Value::String(format!("{PROVIDER_ID}/{ROUTED_MODEL_ID}"));
    if root_object.get("model") != Some(&desired) {
        root_object.insert("model".to_owned(), desired.clone());
        changed = true;
    }
    let providers = object_entry(root_object, "provider", path)?;
    let desired_models = provider_models(context_window);
    match providers.get_mut(PROVIDER_ID) {
        Some(existing) => {
            let existing = existing.as_object_mut().ok_or_else(|| {
                format!(
                    "{}.provider.{PROVIDER_ID} must be a JSON object",
                    path.display()
                )
            })?;
            let desired_npm = Value::String("@ai-sdk/openai-compatible".into());
            if existing.get("npm") != Some(&desired_npm) {
                existing.insert("npm".to_owned(), desired_npm);
                changed = true;
            }
            let desired_name = Value::String("Agent Relay".into());
            if existing.get("name") != Some(&desired_name) {
                existing.insert("name".to_owned(), desired_name);
                changed = true;
            }
            let options = object_entry(existing, "options", path)?;
            let desired_base_url =
                Value::String(client_proxy_base_url(&snapshot.proxy_endpoint, "opencode"));
            if options.get("baseURL") != Some(&desired_base_url) {
                options.insert("baseURL".to_owned(), desired_base_url);
                changed = true;
            }
            if existing.get("models") != Some(&desired_models) {
                existing.insert("models".to_owned(), desired_models);
                changed = true;
            }
        }
        None => {
            providers.insert(PROVIDER_ID.to_owned(), provider(snapshot, context_window));
            changed = true;
        }
    }

    if !changed {
        return Ok(());
    }

    config::preserve_pristine_backup(path, ".agent-relay.bak")?;
    let contents = serde_json::to_string_pretty(&root)
        .map_err(|error| format!("failed to serialize OpenCode configuration: {error}"))?;
    config::atomic_write_text(path, &format!("{contents}\n"))
}

fn object_entry<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, String> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or_else(|| format!("{}.{} must be a JSON object", path.display(), key))
}

fn provider(snapshot: &FleetSnapshot, context_window: u32) -> Value {
    json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "Agent Relay",
        "options": {
            "baseURL": client_proxy_base_url(&snapshot.proxy_endpoint, "opencode")
        },
        "models": provider_models(context_window)
    })
}

fn provider_models(context_window: u32) -> Value {
    let output_window = (context_window / 4).min(16_384);
    json!({
        (ROUTED_MODEL_ID): {
            "name": "Agent Relay",
            "limit": {
                "context": context_window,
                "output": output_window
            }
        }
    })
}

fn error_status(
    path: Option<PathBuf>,
    selected_model: Option<String>,
    error: String,
) -> OpenCodeStatus {
    OpenCodeStatus {
        state: OpenCodeSyncState::Error,
        config_path: path.map(|path| path.display().to_string()),
        model_count: 0,
        selected_model,
        last_synced_at_ms: None,
        error: Some(error),
    }
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
            hosts: vec![
                host(
                    "workstation",
                    "WORKSTATION",
                    ConnectionState::Local,
                    "qwen-gguf",
                ),
                host("air-m4", "Air-M4", ConnectionState::Offline, "qwen-mlx"),
            ],
            opencode: OpenCodeStatus::default(),
            hermes: HermesStatus::default(),
            hermes_cli: HermesStatus::default(),
            codex: HarnessStatus::default(),
            claude_code: HarnessStatus::default(),
            pi: HarnessStatus::default(),
            copilot: HarnessStatus::default(),
            vscode: HarnessStatus::default(),
        }
    }

    fn host(
        id: &str,
        display_name: &str,
        connection: ConnectionState,
        model_id: &str,
    ) -> HostStatus {
        HostStatus {
            id: id.into(),
            display_name: display_name.into(),
            address: id.into(),
            hardware: String::new(),
            connection,
            models: vec![ModelProfile {
                id: model_id.into(),
                display_name: "Qwen".into(),
                runtime: "test".into(),
                kind: Default::default(),
                capabilities: vec![ProfileCapability::Chat],
                lifecycle_adapter: "llama_swap".into(),
                resource_pool: "default".into(),
                context_length: None,
            }],
            loaded_model_id: None,
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
        }
    }

    fn test_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-relay-opencode-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn sync_preserves_unrelated_values_and_includes_offline_models() {
        let directory = test_directory("merge");
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("opencode.jsonc");
        fs::write(
            &path,
            r#"{
              // user-owned setting
              "theme": "system",
              "provider": {
                "other": { "name": "Other" },
                "agentrelay": {
                  "npm": "@ai-sdk/openai-compatible",
                  "name": "Custom fleet name",
                  "options": { "baseURL": "http://127.0.0.1:38475/v1", "timeout": 900000 },
                  "models": {}
                }
              },
            }"#,
        )
        .expect("write config");

        sync_path(&path, &snapshot(), None, 65_536).expect("sync config");
        let written: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read config")).expect("json");
        assert_eq!(written["theme"], "system");
        assert_eq!(written["provider"]["other"]["name"], "Other");
        assert_eq!(written["provider"][PROVIDER_ID]["name"], "Agent Relay");
        assert_eq!(
            written["provider"][PROVIDER_ID]["options"]["timeout"],
            900000
        );
        assert!(written["provider"][PROVIDER_ID]["models"][ROUTED_MODEL_ID].is_object());
        assert_eq!(
            written["provider"][PROVIDER_ID]["options"]["baseURL"],
            "http://127.0.0.1:38475/clients/opencode/v1"
        );
        assert_eq!(
            written["provider"][PROVIDER_ID]["models"][ROUTED_MODEL_ID]["limit"]["context"],
            65_536
        );
        assert!(suffixed_path(&path, ".agent-relay.bak").exists());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn second_sync_does_not_rewrite_an_unchanged_provider() {
        let directory = test_directory("stable");
        let path = directory.join("opencode.json");
        sync_path(&path, &snapshot(), None, 65_536).expect("first sync");
        assert!(!suffixed_path(&path, ".agent-relay.bak").exists());
        sync_path(&path, &snapshot(), None, 65_536).expect("second sync");
        assert!(!suffixed_path(&path, ".agent-relay.bak").exists());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn invalid_provider_shape_is_not_overwritten() {
        let directory = test_directory("invalid");
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("opencode.json");
        let original = r#"{"provider":"invalid"}"#;
        fs::write(&path, original).expect("write config");

        let error =
            sync_path(&path, &snapshot(), None, 65_536).expect_err("reject invalid provider");
        assert!(error.contains("provider must be a JSON object"));
        assert_eq!(fs::read_to_string(&path).expect("read config"), original);

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn selected_fleet_route_keeps_the_virtual_model_as_opencode_default() {
        let directory = test_directory("selected-model");
        let path = directory.join("opencode.json");
        sync_path(&path, &snapshot(), Some("workstation/qwen-gguf"), 65_536)
            .expect("sync selected model");

        let written: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read config")).expect("json");
        assert_eq!(written["model"], "agentrelay/agentrelay");

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn repairs_managed_provider_endpoint_and_preserves_the_pristine_backup() {
        let directory = test_directory("endpoint-repair");
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("opencode.json");
        let original = r#"{
          "theme": "dark",
          "provider": {
            "agentrelay": {
              "npm": "wrong-package",
              "name": "Keep this name",
              "options": {"baseURL": "http://127.0.0.1:9999/v1", "timeout": 42},
              "models": {}
            }
          }
        }"#;
        fs::write(&path, original).expect("write original");

        sync_path(&path, &snapshot(), None, 65_536).expect("repair endpoint");
        let first_written: Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            first_written["provider"][PROVIDER_ID]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            first_written["provider"][PROVIDER_ID]["options"]["baseURL"],
            "http://127.0.0.1:38475/clients/opencode/v1"
        );
        assert_eq!(
            first_written["provider"][PROVIDER_ID]["options"]["timeout"],
            42
        );
        assert_eq!(
            first_written["provider"][PROVIDER_ID]["name"],
            "Agent Relay"
        );

        sync_path(&path, &snapshot(), Some("workstation/qwen-gguf"), 65_536)
            .expect("second managed update");
        assert_eq!(
            fs::read_to_string(suffixed_path(&path, ".agent-relay.bak")).unwrap(),
            original
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn exposes_only_the_virtual_agentrelay_model() {
        let models = provider_models(65_536);
        assert_eq!(models.as_object().expect("model map").len(), 1);
        assert_eq!(models[ROUTED_MODEL_ID]["name"], "Agent Relay");
    }

    #[test]
    fn extracts_only_text_parts_from_opencode_responses() {
        let payload = json!({
            "info": { "role": "assistant" },
            "parts": [
                { "type": "step-start" },
                { "type": "text", "text": "built " },
                { "type": "tool", "state": { "status": "completed" } },
                { "type": "text", "text": "successfully" }
            ]
        });
        assert_eq!(
            opencode_response_text(&payload).as_deref(),
            Some("built successfully")
        );
        assert!(opencode_response_text(&json!({ "parts": [] })).is_none());
    }

    #[test]
    fn bounds_remote_turns_before_outer_gateway_deadlines() {
        assert_eq!(API_LOCK_TIMEOUT, Duration::from_secs(5));
        assert_eq!(TURN_TIMEOUT, Duration::from_secs(25 * 60));
    }

    #[test]
    fn builds_encoded_project_scoped_api_urls() {
        let project = Path::new("C:/Users/TestUser/My Project");
        let url = api_url("session", project).expect("API URL");
        assert_eq!(url.path(), "/session");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("directory".into(), "C:/Users/TestUser/My Project".into())]
        );
    }

    #[test]
    fn reports_nested_opencode_agent_errors() {
        let payload = json!({
            "info": { "error": { "data": { "message": "permission denied" } } }
        });
        assert_eq!(
            api_error(payload.pointer("/info/error").unwrap()).as_deref(),
            Some("permission denied")
        );
    }

    #[test]
    fn selects_the_final_text_from_an_async_opencode_turn() {
        let first = json!({
            "info": { "id": "msg_step", "role": "assistant", "time": { "completed": 2 } },
            "parts": [{ "type": "step-start" }]
        });
        let final_message = json!({
            "info": { "id": "msg_final", "role": "assistant", "time": { "completed": 3 } },
            "parts": [{ "type": "text", "text": "finished the change" }]
        });
        assert_eq!(
            completed_api_reply(&[&first, &final_message]).as_deref(),
            Ok("finished the change")
        );
    }

    #[test]
    fn surfaces_async_opencode_agent_errors() {
        let failed = json!({
            "info": {
                "id": "msg_failed",
                "role": "assistant",
                "error": { "data": { "message": "session is already in flight" } }
            },
            "parts": []
        });
        let error = completed_api_reply(&[&failed]).expect_err("agent error");
        assert_eq!(error, "session is already in flight");
    }

    #[test]
    fn waits_through_opencode_compaction_aborts() {
        let compacted = json!({
            "info": {
                "id": "msg_compaction",
                "role": "assistant",
                "error": {
                    "name": "MessageAbortedError",
                    "data": { "message": "Aborted" }
                }
            },
            "parts": [{ "type": "step-start" }]
        });
        assert!(api_reply_may_continue(&[&compacted]));
    }

    #[test]
    fn does_not_delay_real_opencode_failures_or_text_replies() {
        let failed = json!({
            "info": {
                "id": "msg_failed",
                "role": "assistant",
                "error": { "data": { "message": "permission denied" } }
            },
            "parts": []
        });
        let replied = json!({
            "info": { "id": "msg_replied", "role": "assistant" },
            "parts": [{ "type": "text", "text": "done" }]
        });
        assert!(!api_reply_may_continue(&[&failed]));
        assert!(!api_reply_may_continue(&[&replied]));
    }

    #[test]
    fn reads_root_opencode_sessions_in_recent_order() {
        let directory = test_directory("session-inventory");
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("opencode.db");
        let connection = rusqlite::Connection::open(&path).expect("create OpenCode database");
        connection
            .execute_batch(
                "CREATE TABLE project (id TEXT PRIMARY KEY, name TEXT);\
                 CREATE TABLE session (\
                   id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,\
                   directory TEXT NOT NULL, title TEXT NOT NULL,\
                   time_updated INTEGER NOT NULL, time_archived INTEGER, model TEXT\
                 );\
                 INSERT INTO project VALUES ('game', 'Tower Defense');\
                 INSERT INTO session VALUES\
                   ('ses_old', 'game', NULL, '/Projects/Tower Defense', 'Old plan', 100, NULL, NULL),\
                   ('ses_new', 'game', NULL, '/Projects/Tower Defense', 'Build waves', 300, NULL, '{\"id\":\"agentrelay\",\"providerID\":\"agentrelay\"}'),\
                   ('ses_child', 'game', 'ses_new', '/Projects/Tower Defense', 'Child', 400, NULL, NULL),\
                   ('ses_archived', 'game', NULL, '/Projects/Tower Defense', 'Archived', 200, 250, NULL);",
            )
            .expect("seed OpenCode database");
        drop(connection);

        let mut sessions = read_session_inventory(&path).expect("read session inventory");
        apply_relay_model(&mut sessions, Some("workstation/qwen".into()));
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].id, "ses_new");
        assert_eq!(sessions[0].project_name, "Tower Defense");
        assert_eq!(sessions[0].directory, "/Projects/Tower Defense");
        assert_eq!(sessions[0].provider_id.as_deref(), Some("agentrelay"));
        assert_eq!(sessions[0].model_id.as_deref(), Some("agentrelay"));
        assert_eq!(sessions[0].relay_model.as_deref(), Some("workstation/qwen"));
        assert_eq!(sessions[1].id, "ses_archived");
        assert!(sessions[1].archived);
        assert_eq!(sessions[2].id, "ses_old");
        assert!(sessions[2].relay_model.is_none());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn archives_and_restores_an_opencode_session() {
        let directory = test_directory("session-archive");
        fs::create_dir_all(&directory).expect("create session archive directory");
        let path = directory.join("opencode.db");
        let connection = rusqlite::Connection::open(&path).expect("create OpenCode database");
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, time_archived INTEGER); \
                 INSERT INTO session VALUES ('ses_agentrelay', NULL);",
            )
            .expect("seed OpenCode session");
        drop(connection);

        set_opencode_session_archived(&path, "ses_agentrelay", true)
            .expect("archive OpenCode session");
        let connection = rusqlite::Connection::open(&path).expect("reopen OpenCode database");
        let archived_at: Option<i64> = connection
            .query_row(
                "SELECT time_archived FROM session WHERE id = 'ses_agentrelay'",
                [],
                |row| row.get(0),
            )
            .expect("read OpenCode archive state");
        assert!(archived_at.is_some());
        drop(connection);

        set_opencode_session_archived(&path, "ses_agentrelay", false)
            .expect("restore OpenCode session");
        let connection = rusqlite::Connection::open(&path).expect("reopen OpenCode database");
        let archived_at: Option<i64> = connection
            .query_row(
                "SELECT time_archived FROM session WHERE id = 'ses_agentrelay'",
                [],
                |row| row.get(0),
            )
            .expect("read OpenCode restored state");
        assert!(archived_at.is_none());
        drop(connection);
        fs::remove_dir_all(directory).expect("remove session archive directory");
    }
}
