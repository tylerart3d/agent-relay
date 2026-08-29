use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE_NAME: &str = "fleet.json";
pub const DEFAULT_PEER_API_PORT: u16 = 38_473;
pub const DEFAULT_LLAMA_SWAP_PORT: u16 = 38_474;
pub const DEFAULT_FLEET_PROXY_PORT: u16 = 38_475;
pub const DEFAULT_CLIENT_CONTEXT_WINDOW: u32 = 65_536;
pub const MIN_CLIENT_CONTEXT_WINDOW: u32 = 65_536;
pub const MAX_CLIENT_CONTEXT_WINDOW: u32 = 262_144;
pub const CLIENT_CONTEXT_WINDOW_STEP: u32 = 16_384;
pub const DEFAULT_GATEWAY_FAILOVER_SECONDS: u64 = 60;

static CONFIG_UPDATE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FleetConfig {
    pub schema_version: u8,
    pub local_host_id: String,
    pub poll_interval_seconds: u64,
    pub request_timeout_ms: u64,
    pub peer_api_port: u16,
    #[serde(default)]
    pub llama_swap: LlamaSwapConfig,
    #[serde(default)]
    pub fleet_proxy: FleetProxyConfig,
    #[serde(default)]
    pub opencode: OpenCodeConfig,
    #[serde(default)]
    pub hermes: HermesConfig,
    #[serde(default)]
    pub codex: HarnessConfig,
    #[serde(default)]
    pub claude_code: HarnessConfig,
    #[serde(default)]
    pub pi: HarnessConfig,
    #[serde(default)]
    pub copilot: HarnessConfig,
    #[serde(default)]
    pub vscode: HarnessConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub channel_gateway: ChannelGatewayConfig,
    pub hosts: Vec<HostConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChannelGatewayConfig {
    #[serde(default)]
    pub primary_host_id: Option<String>,
    #[serde(default)]
    pub secondary_host_id: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub automatic_failover: bool,
    #[serde(default = "default_gateway_failover_seconds")]
    pub failover_after_seconds: u64,
    #[serde(default)]
    pub photon_project_id: Option<String>,
    #[serde(default)]
    pub allowed_senders: Vec<String>,
}

impl Default for ChannelGatewayConfig {
    fn default() -> Self {
        Self {
            primary_host_id: None,
            secondary_host_id: None,
            automatic_failover: true,
            failover_after_seconds: DEFAULT_GATEWAY_FAILOVER_SECONDS,
            photon_project_id: None,
            allowed_senders: Vec::new(),
        }
    }
}

fn enabled_by_default() -> bool {
    true
}

fn default_gateway_failover_seconds() -> u64 {
    DEFAULT_GATEWAY_FAILOVER_SECONDS
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    Light,
    Dark,
    #[default]
    System,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UiConfig {
    pub theme: ThemePreference,
    #[serde(default)]
    pub harness_visibility: HarnessVisibility,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HarnessVisibility {
    #[serde(default = "visible_by_default")]
    pub opencode: bool,
    #[serde(default = "visible_by_default")]
    pub opencode_cli: bool,
    #[serde(default = "visible_by_default")]
    pub codex: bool,
    #[serde(default = "visible_by_default")]
    pub claude_code: bool,
    #[serde(default = "visible_by_default")]
    pub copilot: bool,
    #[serde(default = "visible_by_default")]
    pub vscode: bool,
    #[serde(default = "visible_by_default")]
    pub pi: bool,
    #[serde(default = "visible_by_default")]
    pub hermes: bool,
    #[serde(default = "visible_by_default")]
    pub hermes_cli: bool,
}

impl Default for HarnessVisibility {
    fn default() -> Self {
        Self {
            opencode: true,
            opencode_cli: true,
            codex: true,
            claude_code: true,
            copilot: true,
            vscode: true,
            pi: true,
            hermes: true,
            hermes_cli: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OpenCodeConfig {
    pub enabled: bool,
    pub config_path: Option<String>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default = "default_client_context_window")]
    pub context_window: u32,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            config_path: None,
            selected_model: None,
            context_window: DEFAULT_CLIENT_CONTEXT_WINDOW,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HermesConfig {
    pub enabled: bool,
    pub executable_path: Option<String>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub selected_cli_model: Option<String>,
    #[serde(default = "default_client_context_window")]
    pub context_window: u32,
}

impl Default for HermesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable_path: None,
            selected_model: None,
            selected_cli_model: None,
            context_window: DEFAULT_CLIENT_CONTEXT_WINDOW,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct HarnessConfig {
    pub enabled: bool,
    pub config_path: Option<String>,
    #[serde(default)]
    pub selected_model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FleetProxyConfig {
    pub listen_address: String,
}

impl Default for FleetProxyConfig {
    fn default() -> Self {
        Self {
            listen_address: format!("127.0.0.1:{DEFAULT_FLEET_PROXY_PORT}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LlamaSwapConfig {
    pub config_path: String,
    pub listen_address: String,
}

impl Default for LlamaSwapConfig {
    fn default() -> Self {
        Self {
            config_path: "llama-swap.yaml".into(),
            listen_address: format!("127.0.0.1:{DEFAULT_LLAMA_SWAP_PORT}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HostConfig {
    pub id: String,
    pub display_name: String,
    pub address: String,
    pub hardware: String,
}

impl FleetConfig {
    pub fn defaults(machine_name: &str) -> Self {
        let normalized = normalize_host_id(machine_name);
        let local_host_id = normalized.clone();
        let hosts = vec![HostConfig {
            id: local_host_id.clone(),
            display_name: machine_name.trim().to_owned(),
            address: local_host_id.clone(),
            hardware: "Local machine".into(),
        }];

        Self {
            schema_version: 1,
            local_host_id,
            poll_interval_seconds: 5,
            request_timeout_ms: 1_500,
            peer_api_port: DEFAULT_PEER_API_PORT,
            llama_swap: LlamaSwapConfig::default(),
            fleet_proxy: FleetProxyConfig::default(),
            opencode: OpenCodeConfig::default(),
            hermes: HermesConfig::default(),
            codex: HarnessConfig::default(),
            claude_code: HarnessConfig::default(),
            pi: HarnessConfig::default(),
            copilot: HarnessConfig::default(),
            vscode: HarnessConfig::default(),
            ui: UiConfig::default(),
            channel_gateway: ChannelGatewayConfig::default(),
            hosts,
        }
    }

    pub fn load_or_create(config_dir: &Path, machine_name: &str) -> Result<Self, String> {
        let path = config_dir.join(CONFIG_FILE_NAME);
        if path.exists() {
            let contents = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            let value: serde_json::Value = serde_json::from_str(&contents)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            let needs_llama_swap_migration = value.get("llama_swap").is_none();
            let needs_fleet_proxy_migration = value.get("fleet_proxy").is_none();
            let needs_opencode_migration = value.get("opencode").is_none();
            let needs_hermes_migration = value.get("hermes").is_none();
            let needs_codex_migration = value.get("codex").is_none();
            let needs_claude_code_migration = value.get("claude_code").is_none();
            let needs_pi_migration = value.get("pi").is_none();
            let needs_copilot_migration = value.get("copilot").is_none();
            let needs_vscode_migration = value.get("vscode").is_none();
            let needs_ui_migration = value.get("ui").is_none();
            let needs_channel_gateway_migration = value.get("channel_gateway").is_none();
            let needs_context_window_migration =
                value.pointer("/opencode/context_window").is_none()
                    || value.pointer("/hermes/context_window").is_none();
            let needs_hermes_cli_model_migration =
                value.pointer("/hermes/selected_cli_model").is_none();
            let config: Self = serde_json::from_value(value)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            if needs_llama_swap_migration
                || needs_fleet_proxy_migration
                || needs_opencode_migration
                || needs_hermes_migration
                || needs_codex_migration
                || needs_claude_code_migration
                || needs_pi_migration
                || needs_copilot_migration
                || needs_vscode_migration
                || needs_ui_migration
                || needs_channel_gateway_migration
                || needs_context_window_migration
                || needs_hermes_cli_model_migration
            {
                write_config(&path, &config)?;
            }
            return Ok(config);
        }

        fs::create_dir_all(config_dir).map_err(|error| {
            format!(
                "failed to create config directory {}: {error}",
                config_dir.display()
            )
        })?;
        let config = Self::defaults(machine_name);
        write_config(&path, &config)?;
        Ok(config)
    }
}

pub fn get_channel_gateway_config(config_dir: &Path) -> Result<ChannelGatewayConfig, String> {
    Ok(read_config(&config_dir.join(CONFIG_FILE_NAME))?.channel_gateway)
}

pub fn set_channel_gateway_config(
    config_dir: &Path,
    gateway: ChannelGatewayConfig,
) -> Result<ChannelGatewayConfig, String> {
    validate_channel_gateway_config(&gateway)?;
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.channel_gateway = gateway.clone();
    write_config(&path, &config)?;
    Ok(gateway)
}

pub fn validate_channel_gateway_config(gateway: &ChannelGatewayConfig) -> Result<(), String> {
    if gateway
        .primary_host_id
        .as_deref()
        .is_some_and(str::is_empty)
        || gateway
            .secondary_host_id
            .as_deref()
            .is_some_and(str::is_empty)
    {
        return Err("gateway host ids cannot be empty".into());
    }
    if gateway.primary_host_id.is_some() && gateway.primary_host_id == gateway.secondary_host_id {
        return Err("primary and secondary gateway hosts must be different".into());
    }
    if !(15..=600).contains(&gateway.failover_after_seconds) {
        return Err("gateway failover delay must be between 15 and 600 seconds".into());
    }
    if gateway
        .photon_project_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("Photon project id cannot be empty".into());
    }
    if gateway
        .allowed_senders
        .iter()
        .any(|sender| sender.trim().is_empty())
    {
        return Err("allowed Photon senders cannot be empty".into());
    }
    Ok(())
}

pub fn set_opencode_enabled(config_dir: &Path, enabled: bool) -> Result<OpenCodeConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut config: FleetConfig = serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    config.opencode.enabled = enabled;
    write_config(&path, &config)?;
    Ok(config.opencode)
}

pub fn set_hermes_enabled(config_dir: &Path, enabled: bool) -> Result<HermesConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut config: FleetConfig = serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    config.hermes.enabled = enabled;
    write_config(&path, &config)?;
    Ok(config.hermes)
}

pub fn set_opencode_model(
    config_dir: &Path,
    selected_model: String,
) -> Result<OpenCodeConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.opencode.enabled = true;
    config.opencode.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.opencode)
}

pub fn set_hermes_model(config_dir: &Path, selected_model: String) -> Result<HermesConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.hermes.enabled = true;
    config.hermes.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.hermes)
}

pub fn set_hermes_cli_model(
    config_dir: &Path,
    selected_model: String,
) -> Result<HermesConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.hermes.enabled = true;
    config.hermes.selected_cli_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.hermes)
}

pub fn set_codex_model(config_dir: &Path, selected_model: String) -> Result<HarnessConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.codex.enabled = true;
    config.codex.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.codex)
}

pub fn set_claude_code_model(
    config_dir: &Path,
    selected_model: String,
) -> Result<HarnessConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.claude_code.enabled = true;
    config.claude_code.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.claude_code)
}

pub fn set_pi_model(config_dir: &Path, selected_model: String) -> Result<HarnessConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.pi.enabled = true;
    config.pi.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.pi)
}

pub fn set_copilot_model(
    config_dir: &Path,
    selected_model: String,
) -> Result<HarnessConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.copilot.enabled = true;
    config.copilot.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.copilot)
}

pub fn set_vscode_model(
    config_dir: &Path,
    selected_model: String,
) -> Result<HarnessConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.vscode.enabled = true;
    config.vscode.selected_model = Some(selected_model);
    write_config(&path, &config)?;
    Ok(config.vscode)
}

pub fn set_opencode_context_window(
    config_dir: &Path,
    context_window: u32,
) -> Result<OpenCodeConfig, String> {
    validate_client_context_window(context_window)?;
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.opencode.context_window = context_window;
    write_config(&path, &config)?;
    Ok(config.opencode)
}

pub fn set_hermes_context_window(
    config_dir: &Path,
    context_window: u32,
) -> Result<HermesConfig, String> {
    validate_client_context_window(context_window)?;
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.hermes.context_window = context_window;
    write_config(&path, &config)?;
    Ok(config.hermes)
}

pub fn get_client_context_windows(config_dir: &Path) -> Result<(u32, u32), String> {
    let config = read_config(&config_dir.join(CONFIG_FILE_NAME))?;
    Ok((config.hermes.context_window, config.opencode.context_window))
}

pub fn get_ui_config(config_dir: &Path) -> Result<UiConfig, String> {
    Ok(read_config(&config_dir.join(CONFIG_FILE_NAME))?.ui)
}

pub fn set_theme(config_dir: &Path, theme: ThemePreference) -> Result<UiConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    config.ui.theme = theme;
    write_config(&path, &config)?;
    Ok(config.ui)
}

pub fn set_harness_visible(
    config_dir: &Path,
    harness: &str,
    visible: bool,
) -> Result<UiConfig, String> {
    let _guard = lock_config_updates()?;
    let path = config_dir.join(CONFIG_FILE_NAME);
    let mut config = read_config(&path)?;
    let visibility = &mut config.ui.harness_visibility;
    match harness {
        "opencode" => visibility.opencode = visible,
        "opencode_cli" => visibility.opencode_cli = visible,
        "codex" => visibility.codex = visible,
        "claude_code" => visibility.claude_code = visible,
        "copilot" => visibility.copilot = visible,
        "vscode" => visibility.vscode = visible,
        "pi" => visibility.pi = visible,
        "hermes" => visibility.hermes = visible,
        "hermes_cli" => visibility.hermes_cli = visible,
        _ => return Err(format!("unknown harness '{harness}'")),
    }
    write_config(&path, &config)?;
    Ok(config.ui)
}

fn read_config(path: &Path) -> Result<FleetConfig, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn write_config(path: &Path, config: &FleetConfig) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(config)
        .map_err(|error| format!("failed to serialize fleet config: {error}"))?;
    atomic_write_text(path, &format!("{contents}\n"))
}

fn lock_config_updates() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    CONFIG_UPDATE_LOCK
        .lock()
        .map_err(|_| "fleet config update lock poisoned".to_owned())
}

pub(crate) fn preserve_pristine_backup(path: &Path, suffix: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    let backup = suffixed_path(path, suffix);
    if backup.exists() {
        return Ok(());
    }
    let temporary = temporary_path(&backup);
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            format!(
                "failed to prepare pristine backup {}: {error}",
                backup.display()
            )
        })?;
    let copy_result = (|| -> io::Result<()> {
        let mut source = File::open(path)?;
        io::copy(&mut source, &mut destination)?;
        destination.sync_all()
    })();
    if let Err(error) = copy_result {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "failed to preserve pristine backup {}: {error}",
            backup.display()
        ));
    }
    drop(destination);
    match install_new_file(&temporary, &backup) {
        Ok(()) => {
            let parent = backup.parent().unwrap_or_else(|| Path::new("."));
            sync_parent_directory(parent).map_err(|error| {
                format!(
                    "failed to make pristine backup {} durable: {error}",
                    backup.display()
                )
            })
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(format!(
                "failed to install pristine backup {}: {error}",
                backup.display()
            ))
        }
    }
}

pub(crate) fn atomic_write_text(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;

    let temporary = temporary_path(path);
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "failed to atomically write {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".agentrelay-{}-{timestamp}-{sequence}.tmp",
        std::process::id()
    ));
    path.with_file_name(name)
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(not(windows))]
fn install_new_file(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(temporary, target)?;
    let _ = fs::remove_file(temporary);
    Ok(())
}

#[cfg(windows)]
fn install_new_file(temporary: &Path, target: &Path) -> io::Result<()> {
    move_file(temporary, target, MOVEFILE_WRITE_THROUGH)
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    move_file(
        temporary,
        target,
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    )
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

#[cfg(windows)]
fn move_file(temporary: &Path, target: &Path, flags: u32) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let existing = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe { MoveFileExW(existing.as_ptr(), replacement.as_ptr(), flags) };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "Kernel32")]
extern "system" {
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

#[cfg(not(windows))]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

fn default_client_context_window() -> u32 {
    DEFAULT_CLIENT_CONTEXT_WINDOW
}

fn visible_by_default() -> bool {
    true
}

pub fn validate_client_context_window(context_window: u32) -> Result<(), String> {
    if !(MIN_CLIENT_CONTEXT_WINDOW..=MAX_CLIENT_CONTEXT_WINDOW).contains(&context_window)
        || !(context_window - MIN_CLIENT_CONTEXT_WINDOW).is_multiple_of(CLIENT_CONTEXT_WINDOW_STEP)
    {
        return Err(format!(
            "context window must be between {MIN_CLIENT_CONTEXT_WINDOW} and {MAX_CLIENT_CONTEXT_WINDOW} tokens in {CLIENT_CONTEXT_WINDOW_STEP}-token steps"
        ));
    }
    Ok(())
}

fn normalize_host_id(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_local_id_from_the_machine_name() {
        assert_eq!(
            FleetConfig::defaults("GPU Workstation").local_host_id,
            "gpu-workstation"
        );
        assert_eq!(
            FleetConfig::defaults("Studio-Mac").local_host_id,
            "studio-mac"
        );
    }

    #[test]
    fn adds_an_unknown_machine_to_its_own_default_host_catalog() {
        let config = FleetConfig::defaults("Studio Ultra");
        assert_eq!(config.local_host_id, "studio-ultra");
        let local = config
            .hosts
            .iter()
            .find(|host| host.id == config.local_host_id)
            .expect("unknown local host should be represented");
        assert_eq!(local.display_name, "Studio Ultra");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(local.address, "studio-ultra");
    }

    #[test]
    fn preserves_only_the_first_pristine_backup() {
        let directory = std::env::temp_dir().join(format!(
            "agent-relay-pristine-backup-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("client.json");
        fs::write(&path, "original").expect("write original");

        preserve_pristine_backup(&path, ".bak").expect("first backup");
        atomic_write_text(&path, "managed once").expect("first managed write");
        preserve_pristine_backup(&path, ".bak").expect("second backup is a no-op");
        atomic_write_text(&path, "managed twice").expect("second managed write");

        assert_eq!(fs::read_to_string(&path).unwrap(), "managed twice");
        assert_eq!(
            fs::read_to_string(suffixed_path(&path, ".bak")).unwrap(),
            "original"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn creates_and_reloads_default_config() {
        let directory =
            std::env::temp_dir().join(format!("agent-relay-config-test-{}", std::process::id()));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }

        let created = FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");
        let loaded = FleetConfig::load_or_create(&directory, "ignored").expect("reload");
        assert_eq!(created, loaded);
        assert!(directory.join(CONFIG_FILE_NAME).exists());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn migrates_config_created_before_service_settings() {
        let directory = std::env::temp_dir().join(format!(
            "agent-relay-config-migration-test-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        fs::create_dir_all(&directory).expect("create test directory");
        let mut legacy =
            serde_json::to_value(FleetConfig::defaults("WORKSTATION")).expect("serialize defaults");
        legacy
            .as_object_mut()
            .expect("config object")
            .remove("llama_swap");
        legacy
            .as_object_mut()
            .expect("config object")
            .remove("fleet_proxy");
        legacy
            .as_object_mut()
            .expect("config object")
            .remove("opencode");
        legacy
            .as_object_mut()
            .expect("config object")
            .remove("hermes");
        legacy.as_object_mut().expect("config object").remove("ui");
        fs::write(
            directory.join(CONFIG_FILE_NAME),
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy config"),
        )
        .expect("write legacy config");

        let migrated = FleetConfig::load_or_create(&directory, "WORKSTATION").expect("migrate");
        let persisted: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(directory.join(CONFIG_FILE_NAME)).expect("read migrated config"),
        )
        .expect("parse migrated config");
        assert_eq!(migrated.llama_swap, LlamaSwapConfig::default());
        assert!(persisted.get("llama_swap").is_some());
        assert!(persisted.get("fleet_proxy").is_some());
        assert_eq!(migrated.opencode, OpenCodeConfig::default());
        assert!(persisted.get("opencode").is_some());
        assert_eq!(migrated.hermes, HermesConfig::default());
        assert!(persisted.get("hermes").is_some());
        assert_eq!(migrated.ui, UiConfig::default());
        assert!(persisted.get("ui").is_some());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn updates_opencode_without_changing_other_fleet_settings() {
        let directory = std::env::temp_dir().join(format!(
            "agent-relay-opencode-toggle-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        let original = FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");

        let updated = set_opencode_enabled(&directory, true).expect("enable OpenCode");
        let reloaded = FleetConfig::load_or_create(&directory, "ignored").expect("reload");
        assert!(updated.enabled);
        assert!(reloaded.opencode.enabled);
        assert_eq!(reloaded.hosts, original.hosts);
        assert_eq!(reloaded.llama_swap, original.llama_swap);

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn updates_hermes_without_changing_other_fleet_settings() {
        let directory =
            std::env::temp_dir().join(format!("agent-relay-hermes-toggle-{}", std::process::id()));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        let original = FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");

        let updated = set_hermes_enabled(&directory, false).expect("disable Hermes sync");
        let reloaded = FleetConfig::load_or_create(&directory, "ignored").expect("reload");
        assert!(!updated.enabled);
        assert!(!reloaded.hermes.enabled);
        assert_eq!(reloaded.hosts, original.hosts);
        assert_eq!(reloaded.llama_swap, original.llama_swap);

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn persists_hermes_desktop_and_cli_selections_independently() {
        let directory = std::env::temp_dir().join(format!(
            "agent-relay-hermes-selections-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");

        set_hermes_model(&directory, "workstation/desktop".into()).expect("select desktop");
        set_hermes_cli_model(&directory, "m1-pro/cli".into()).expect("select CLI");
        let reloaded = FleetConfig::load_or_create(&directory, "ignored").expect("reload");
        assert_eq!(
            reloaded.hermes.selected_model.as_deref(),
            Some("workstation/desktop")
        );
        assert_eq!(
            reloaded.hermes.selected_cli_model.as_deref(),
            Some("m1-pro/cli")
        );

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn updates_theme_without_changing_other_fleet_settings() {
        let directory =
            std::env::temp_dir().join(format!("agent-relay-theme-toggle-{}", std::process::id()));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        let original = FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");

        let updated = set_theme(&directory, ThemePreference::Light).expect("set theme");
        let reloaded = FleetConfig::load_or_create(&directory, "ignored").expect("reload");
        assert_eq!(updated.theme, ThemePreference::Light);
        assert_eq!(reloaded.ui.theme, ThemePreference::Light);
        assert_eq!(reloaded.hosts, original.hosts);
        assert_eq!(reloaded.llama_swap, original.llama_swap);

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn persists_harness_visibility_without_disabling_the_harness() {
        let directory = std::env::temp_dir().join(format!(
            "agent-relay-harness-visibility-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");

        let updated = set_harness_visible(&directory, "codex", false).expect("hide Codex");
        let reloaded = FleetConfig::load_or_create(&directory, "ignored").expect("reload");
        assert!(!updated.harness_visibility.codex);
        assert!(!reloaded.ui.harness_visibility.codex);
        assert!(reloaded.ui.harness_visibility.hermes);
        assert!(reloaded.ui.harness_visibility.hermes_cli);
        assert!(reloaded.ui.harness_visibility.opencode_cli);
        assert!(!reloaded.codex.enabled);
        assert!(set_harness_visible(&directory, "unknown", false).is_err());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn updates_client_context_windows_and_rejects_invalid_steps() {
        let directory =
            std::env::temp_dir().join(format!("agent-relay-context-window-{}", std::process::id()));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove stale test directory");
        }
        FleetConfig::load_or_create(&directory, "WORKSTATION").expect("create");

        let hermes = set_hermes_context_window(&directory, 131_072).expect("set Hermes context");
        let opencode =
            set_opencode_context_window(&directory, 262_144).expect("set OpenCode context");
        assert_eq!(hermes.context_window, 131_072);
        assert_eq!(opencode.context_window, 262_144);
        assert!(set_hermes_context_window(&directory, 70_000).is_err());
        assert_eq!(
            get_client_context_windows(&directory).expect("read contexts"),
            (131_072, 262_144)
        );

        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
