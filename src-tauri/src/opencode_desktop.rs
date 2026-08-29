use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusty_leveldb::{Options as LevelDbOptions, DB};
use serde_json::{json, Value};

use crate::config;
use crate::fleet_proxy::ROUTED_MODEL_ID;

#[cfg(windows)]
use std::collections::HashSet;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use sysinfo::{ProcessesToUpdate, System};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const PROVIDER_ID: &str = "agentrelay";
const RECENT_MODEL_LIMIT: usize = 5;
const OPENCODE_RENDERER_ORIGIN: &str = "oc://renderer";
const MANAGED_SERVER_URL: &str = "http://127.0.0.1:38476";

pub async fn relaunch(selected_model: &str) -> Result<(), String> {
    validate_selected_model(selected_model)?;
    let application = resolve_application()?;
    request_quit(&application)?;

    for _ in 0..50 {
        if !is_running(&application)? {
            return prepare_and_launch(&application, selected_model);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for _ in 0..2 {
        force_quit(&application)?;
        for _ in 0..20 {
            if !is_running(&application)? {
                return prepare_and_launch(&application, selected_model);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let survivors = surviving_process_description(&application);
    Err(format!(
        "OpenCode Desktop did not close; remaining processes: {survivors}"
    ))
}

pub async fn ensure_virtual_model() -> Result<(), String> {
    let data_dir = application_data_dir()?;
    if desktop_model_state_is_current(&data_dir, ROUTED_MODEL_ID)?
        && desktop_server_state_is_current(&data_dir)?
    {
        return Ok(());
    }
    relaunch(ROUTED_MODEL_ID).await
}

pub async fn refresh_running_desktop() -> Result<bool, String> {
    let application = resolve_application()?;
    if !is_running(&application)? {
        return Ok(false);
    }
    ensure_virtual_model().await?;
    Ok(true)
}

fn prepare_and_launch(application: &PathBuf, selected_model: &str) -> Result<(), String> {
    prepare_new_session(&application_data_dir()?, selected_model)?;
    launch(application)
}

fn validate_selected_model(selected_model: &str) -> Result<(), String> {
    if selected_model.trim().is_empty() {
        return Err("OpenCode model ID cannot be empty".into());
    }
    Ok(())
}

fn prepare_new_session(data_dir: &Path, selected_model: &str) -> Result<(), String> {
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("failed to create {}: {error}", data_dir.display()))?;
    let global_path = data_dir.join("opencode.global.dat");
    let mut global = if global_path.exists() {
        read_json(&global_path)?
    } else {
        json!({})
    };
    patch_global_model_selection(&mut global, selected_model)?;
    patch_global_server(&mut global)?;

    preserve_backup(&global_path)?;
    write_json(&global_path, &global)?;
    patch_default_server(data_dir)?;
    patch_window_servers(data_dir)?;
    focus_new_session(data_dir, &global)?;
    Ok(())
}

fn patch_default_server(data_dir: &Path) -> Result<(), String> {
    let path = data_dir.join("opencode.settings");
    let mut settings = if path.exists() {
        read_json(&path)?
    } else {
        json!({})
    };
    settings
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop settings must contain a JSON object".to_owned())?
        .insert(
            "defaultServerUrl".into(),
            Value::String(MANAGED_SERVER_URL.into()),
        );
    preserve_named_backup(&path, "opencode.settings.agentrelay-backup")?;
    write_json(&path, &settings)
}

fn patch_global_server(global: &mut Value) -> Result<(), String> {
    let global = global
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop global state must contain a JSON object".to_owned())?;
    let mut state = match global.get("server") {
        Some(Value::String(contents)) => serde_json::from_str::<Value>(contents)
            .map_err(|error| format!("failed to parse OpenCode Desktop server state: {error}"))?,
        Some(_) => return Err("OpenCode Desktop server state must be a JSON string".into()),
        None => json!({}),
    };
    let state = state
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop server state must contain a JSON object".to_owned())?;
    let list = state
        .entry("list")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| "OpenCode Desktop server list must be an array".to_owned())?;
    list.retain(|entry| server_entry_url(entry) != Some(MANAGED_SERVER_URL));
    list.insert(
        0,
        json!({
            "type": "http",
            "http": { "url": MANAGED_SERVER_URL },
            "displayName": "Agent Relay"
        }),
    );

    for key in ["projects", "lastProject", "recentlyClosed"] {
        let values = state.entry(key).or_insert_with(|| json!({}));
        let values = values
            .as_object_mut()
            .ok_or_else(|| format!("OpenCode Desktop server state {key} must be an object"))?;
        if !values.contains_key(MANAGED_SERVER_URL) {
            if let Some(local) = values.get("local").cloned() {
                values.insert(MANAGED_SERVER_URL.into(), local);
            }
        }
    }
    global.insert(
        "server".into(),
        Value::String(
            serde_json::to_string(state)
                .map_err(|error| format!("failed to serialize OpenCode server state: {error}"))?,
        ),
    );
    Ok(())
}

fn server_entry_url(entry: &Value) -> Option<&str> {
    entry
        .as_str()
        .or_else(|| entry.pointer("/http/url").and_then(Value::as_str))
        .or_else(|| entry.get("url").and_then(Value::as_str))
}

fn patch_window_servers(data_dir: &Path) -> Result<(), String> {
    let Some(path) = newest_window_state(data_dir)? else {
        return Ok(());
    };
    let mut window = read_json(&path)?;
    let window_object = window
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop window state must contain a JSON object".to_owned())?;
    let mut tabs: Vec<Value> = window_object
        .get("tabs")
        .and_then(Value::as_str)
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            format!(
                "failed to parse OpenCode tabs in {}: {error}",
                path.display()
            )
        })?
        .unwrap_or_default();
    for tab in &mut tabs {
        if let Some(server) = tab.get_mut("server") {
            if server.as_str() == Some("sidecar") {
                *server = Value::String(MANAGED_SERVER_URL.into());
            }
        }
    }
    window_object.insert(
        "tabs".into(),
        Value::String(serde_json::to_string(&tabs).map_err(|error| {
            format!(
                "failed to serialize OpenCode tabs in {}: {error}",
                path.display()
            )
        })?),
    );

    let mut key_map = std::collections::HashMap::new();
    for tab in &tabs {
        if tab.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let Some(session_id) = tab.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let old = session_tab_key("sidecar", session_id);
        let new = session_tab_key(MANAGED_SERVER_URL, session_id);
        key_map.insert(old, new);
    }
    rewrite_window_tab_keys(window_object, "tabs.recent", &key_map)?;
    rewrite_window_tab_info(window_object, &key_map)?;

    preserve_named_backup(
        &path,
        &format!(
            "{}.agentrelay-backup",
            path.file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("opencode.window.dat")
        ),
    )?;
    write_json(&path, &window)
}

fn session_tab_key(server: &str, session_id: &str) -> String {
    format!(
        "{server}\n/server/{}/session/{session_id}",
        URL_SAFE_NO_PAD.encode(server.as_bytes())
    )
}

fn rewrite_window_tab_keys(
    window: &mut serde_json::Map<String, Value>,
    field: &str,
    key_map: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let Some(Value::String(contents)) = window.get(field) else {
        return Ok(());
    };
    let mut value: Value = serde_json::from_str(contents)
        .map_err(|error| format!("failed to parse OpenCode {field}: {error}"))?;
    if let Some(key) = value.get_mut("key") {
        if let Some(replacement) = key.as_str().and_then(|old| key_map.get(old)) {
            *key = Value::String(replacement.clone());
        }
    }
    window.insert(
        field.into(),
        Value::String(
            serde_json::to_string(&value)
                .map_err(|error| format!("failed to serialize OpenCode {field}: {error}"))?,
        ),
    );
    Ok(())
}

fn rewrite_window_tab_info(
    window: &mut serde_json::Map<String, Value>,
    key_map: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let Some(Value::String(contents)) = window.get("tabs.info") else {
        return Ok(());
    };
    let mut info: serde_json::Map<String, Value> = serde_json::from_str(contents)
        .map_err(|error| format!("failed to parse OpenCode tabs.info: {error}"))?;
    for (old, new) in key_map {
        if let Some(value) = info.remove(old) {
            info.insert(new.clone(), value);
        }
    }
    window.insert(
        "tabs.info".into(),
        Value::String(
            serde_json::to_string(&info)
                .map_err(|error| format!("failed to serialize OpenCode tabs.info: {error}"))?,
        ),
    );
    Ok(())
}

fn focus_new_session(data_dir: &Path, global: &Value) -> Result<(), String> {
    let Some(window_path) = newest_window_state(data_dir)? else {
        return Err(
            "Open OpenCode Desktop once, close it, then connect the model again so Agent Relay can select its new-session window"
                .into(),
        );
    };
    let window = read_json(&window_path)?;
    let tabs_text = window
        .get("tabs")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("OpenCode tab state is missing in {}", window_path.display()))?;
    let tabs: Vec<Value> = serde_json::from_str(tabs_text).map_err(|error| {
        format!(
            "failed to parse OpenCode tabs in {}: {error}",
            window_path.display()
        )
    })?;
    let recent_key = window
        .get("tabs.recent")
        .and_then(Value::as_str)
        .map(serde_json::from_str::<Value>)
        .transpose()
        .map_err(|error| {
            format!(
                "failed to parse OpenCode active tab in {}: {error}",
                window_path.display()
            )
        })?
        .and_then(|recent| recent.get("key").and_then(Value::as_str).map(str::to_owned));

    let active = recent_key
        .as_deref()
        .and_then(|key| tabs.iter().find(|tab| tab_matches_key(tab, key)))
        .or_else(|| tabs.last());
    let server = active
        .and_then(|tab| tab.get("server"))
        .and_then(Value::as_str)
        .unwrap_or("sidecar")
        .to_owned();
    let directory = active
        .and_then(|tab| tab.get("directory"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            recent_key
                .as_deref()
                .and_then(|key| tab_info_directory(&window, key))
        })
        .or_else(|| {
            tabs.iter().rev().find_map(|tab| {
                (tab.get("server").and_then(Value::as_str) == Some(server.as_str()))
                    .then(|| {
                        tab.get("directory")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
            })
        })
        .or_else(|| global_last_project(global))
        .ok_or_else(|| {
            "OpenCode Desktop could not determine which project to use for the new session"
                .to_owned()
        })?;

    let route = format!("/{}/session", URL_SAFE_NO_PAD.encode(directory.as_bytes()));
    focus_new_session_route(data_dir, &window_path, &route)
}

fn focus_new_session_route(data_dir: &Path, window_path: &Path, route: &str) -> Result<(), String> {
    let window_id = window_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("opencode.window."))
        .and_then(|name| name.strip_suffix(".dat"))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            format!(
                "cannot identify the OpenCode Desktop window from {}",
                window_path.display()
            )
        })?;
    let storage_root = data_dir.join("Local Storage");
    let storage_path = storage_root.join("leveldb");
    let previous_path = storage_root.join("leveldb.agentrelay-previous");
    recover_navigation_storage(&storage_path, &previous_path)?;
    if !storage_path.is_dir() {
        return Err(format!(
            "OpenCode Desktop navigation state is missing at {}",
            storage_path.display()
        ));
    }
    let pristine_backup = storage_root.join("leveldb.agentrelay-backup");
    preserve_directory_backup(&storage_path, &pristine_backup)?;
    let staging_path = storage_root.join(format!(
        "leveldb.agentrelay-staging-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    copy_directory_tree(&storage_path, &staging_path).map_err(|error| {
        format!(
            "failed to stage OpenCode Desktop navigation state at {}: {error}",
            staging_path.display()
        )
    })?;

    let key = chromium_local_storage_key(
        OPENCODE_RENDERER_ORIGIN,
        &format!("opencode.desktop.window.{window_id}.last-active-url"),
    );
    let value = chromium_local_storage_string(route);
    if let Err(error) = patch_navigation_storage(&staging_path, &key, &value) {
        let _ = fs::remove_dir_all(&staging_path);
        return Err(error);
    }

    replace_navigation_storage(&storage_path, &staging_path, &previous_path)
}

fn recover_navigation_storage(storage_path: &Path, previous_path: &Path) -> Result<(), String> {
    if storage_path.exists() || !previous_path.exists() {
        return Ok(());
    }
    fs::rename(previous_path, storage_path).map_err(|error| {
        format!(
            "failed to recover OpenCode Desktop navigation state from {}: {error}",
            previous_path.display()
        )
    })
}

fn preserve_directory_backup(source: &Path, backup: &Path) -> Result<(), String> {
    if backup.exists() {
        return backup.is_dir().then_some(()).ok_or_else(|| {
            format!(
                "OpenCode Desktop navigation backup is not a directory: {}",
                backup.display()
            )
        });
    }
    let parent = backup
        .parent()
        .ok_or_else(|| format!("invalid navigation backup path: {}", backup.display()))?;
    let staging = parent.join(format!(
        "leveldb.agentrelay-backup-staging-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if let Err(error) = copy_directory_tree(source, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "failed to stage the OpenCode Desktop navigation backup at {}: {error}",
            staging.display()
        ));
    }
    if let Err(error) = fs::rename(&staging, backup) {
        let _ = fs::remove_dir_all(&staging);
        if backup.is_dir() {
            return Ok(());
        }
        return Err(format!(
            "failed to publish the OpenCode Desktop navigation backup at {}: {error}",
            backup.display()
        ));
    }
    Ok(())
}

fn replace_navigation_storage(
    storage_path: &Path,
    staging_path: &Path,
    previous_path: &Path,
) -> Result<(), String> {
    if previous_path.exists() {
        if let Err(error) = fs::remove_dir_all(previous_path) {
            let _ = fs::remove_dir_all(staging_path);
            return Err(format!(
                "failed to replace the previous OpenCode Desktop navigation backup at {}: {error}",
                previous_path.display()
            ));
        }
    }
    if let Err(error) = fs::rename(storage_path, previous_path) {
        let _ = fs::remove_dir_all(staging_path);
        return Err(format!(
            "failed to preserve the current OpenCode Desktop navigation state at {}: {error}",
            previous_path.display()
        ));
    }
    if let Err(error) = fs::rename(staging_path, storage_path) {
        let restore = fs::rename(previous_path, storage_path);
        let _ = fs::remove_dir_all(staging_path);
        return match restore {
            Ok(()) => Err(format!(
                "failed to install the new OpenCode Desktop navigation state: {error}"
            )),
            Err(restore_error) => Err(format!(
                "failed to install the new OpenCode Desktop navigation state: {error}; failed to restore the original state: {restore_error}"
            )),
        };
    }
    Ok(())
}

fn patch_navigation_storage(storage_path: &Path, key: &[u8], value: &[u8]) -> Result<(), String> {
    let options = LevelDbOptions {
        create_if_missing: false,
        paranoid_checks: true,
        ..LevelDbOptions::default()
    };
    let mut storage = DB::open(storage_path, options).map_err(|error| {
        format!(
            "failed to open OpenCode Desktop navigation state at {}: {error}",
            storage_path.display()
        )
    })?;
    let snapshot = storage.get_snapshot();
    let metadata = storage
        .get_at(
            &snapshot,
            format!("META:{OPENCODE_RENDERER_ORIGIN}").as_bytes(),
        )
        .map_err(|error| {
            format!("failed to validate OpenCode Desktop navigation state: {error}")
        })?;
    if metadata.is_none() {
        return Err("OpenCode Desktop navigation state uses an unsupported storage format".into());
    }
    storage
        .put(key, value)
        .map_err(|error| format!("failed to select the new OpenCode Desktop session: {error}"))?;
    storage
        .flush()
        .map_err(|error| format!("failed to save the new OpenCode Desktop session: {error}"))?;
    let snapshot = storage.get_snapshot();
    let stored = storage
        .get_at(&snapshot, key)
        .map_err(|error| format!("failed to verify the new OpenCode Desktop session: {error}"))?;
    if stored.as_deref() != Some(value) {
        return Err("OpenCode Desktop did not save the new session selection".into());
    }
    storage
        .close()
        .map_err(|error| format!("failed to close OpenCode Desktop navigation state: {error}"))
}

fn copy_directory_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_directory_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unsupported entry in OpenCode Desktop navigation state: {}",
                    source_path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn chromium_local_storage_key(origin: &str, name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(origin.len() + name.len() + 3);
    key.push(b'_');
    key.extend_from_slice(origin.as_bytes());
    key.push(0);
    key.push(1);
    key.extend_from_slice(name.as_bytes());
    key
}

fn chromium_local_storage_string(value: &str) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(value.len() + 1);
    encoded.push(1);
    encoded.extend_from_slice(value.as_bytes());
    encoded
}

fn newest_window_state(data_dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut windows = fs::read_dir(data_dir)
        .map_err(|error| format!("failed to read OpenCode Desktop state: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("opencode.window.") && name.ends_with(".dat"))
        })
        .collect::<Vec<_>>();
    if windows.len() > 1 {
        return Err(
            "OpenCode Desktop has multiple saved windows; close the extra windows and reconnect so Agent Relay does not switch the wrong one"
                .into(),
        );
    }
    windows.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    Ok(windows.pop())
}

fn tab_matches_key(tab: &Value, key: &str) -> bool {
    match tab.get("type").and_then(Value::as_str) {
        Some("draft") => tab
            .get("draftID")
            .and_then(Value::as_str)
            .is_some_and(|id| key == format!("draft:{id}")),
        Some("session") => {
            let Some(server) = tab.get("server").and_then(Value::as_str) else {
                return false;
            };
            let Some(session_id) = tab.get("sessionId").and_then(Value::as_str) else {
                return false;
            };
            key.starts_with(&format!("{server}\n"))
                && key.ends_with(&format!("/session/{session_id}"))
        }
        _ => false,
    }
}

fn tab_info_directory(window: &Value, key: &str) -> Option<String> {
    let info_text = window.get("tabs.info")?.as_str()?;
    let info: Value = serde_json::from_str(info_text).ok()?;
    info.get(key)?.get("directory")?.as_str().map(str::to_owned)
}

fn global_last_project(global: &Value) -> Option<String> {
    let server_text = global.get("server")?.as_str()?;
    let server: Value = serde_json::from_str(server_text).ok()?;
    server
        .pointer("/lastProject/local")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn patch_global_model_selection(global: &mut Value, selected_model: &str) -> Result<(), String> {
    let global = global
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop global state must contain a JSON object".to_owned())?;
    let mut model_state = match global.get("model") {
        Some(Value::String(contents)) => serde_json::from_str::<Value>(contents)
            .map_err(|error| format!("failed to parse OpenCode Desktop model state: {error}"))?,
        Some(_) => return Err("OpenCode Desktop model state must be a JSON string".into()),
        None => json!({}),
    };
    let model_state = model_state
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop model state must contain a JSON object".to_owned())?;

    remove_managed_models(model_state, "user")?;
    remove_managed_models(model_state, "recent")?;

    promote_model(
        model_state,
        "user",
        json!({
            "providerID": PROVIDER_ID,
            "modelID": selected_model,
            "visibility": "show"
        }),
        selected_model,
        None,
    )?;
    promote_model(
        model_state,
        "recent",
        json!({ "providerID": PROVIDER_ID, "modelID": selected_model }),
        selected_model,
        Some(RECENT_MODEL_LIMIT),
    )?;
    let variant = model_state.entry("variant").or_insert_with(|| json!({}));
    let variant = variant
        .as_object_mut()
        .ok_or_else(|| "OpenCode Desktop model variants must contain a JSON object".to_owned())?;
    variant.retain(|key, _| {
        key.split_once('/')
            .is_none_or(|(provider, _)| !is_managed_provider(provider))
    });
    global.insert(
        "model".into(),
        Value::String(
            serde_json::to_string(&model_state)
                .map_err(|error| format!("failed to serialize OpenCode model state: {error}"))?,
        ),
    );
    Ok(())
}

fn remove_managed_models(
    model_state: &mut serde_json::Map<String, Value>,
    key: &str,
) -> Result<(), String> {
    let values = model_state
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| format!("OpenCode Desktop model state {key} must be an array"))?;
    values.retain(|candidate| {
        candidate
            .get("providerID")
            .and_then(Value::as_str)
            .is_none_or(|provider| !is_managed_provider(provider))
    });
    Ok(())
}

fn is_managed_provider(provider: &str) -> bool {
    provider == PROVIDER_ID
}

fn desktop_model_state_is_current(data_dir: &Path, selected_model: &str) -> Result<bool, String> {
    let path = data_dir.join("opencode.global.dat");
    if !path.is_file() {
        return Ok(false);
    }
    let global = read_json(&path)?;
    let Some(contents) = global.get("model").and_then(Value::as_str) else {
        return Ok(false);
    };
    let model_state: Value = serde_json::from_str(contents)
        .map_err(|error| format!("failed to parse OpenCode Desktop model state: {error}"))?;
    let selected = |key: &str| {
        model_state
            .get(key)
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .is_some_and(|candidate| {
                candidate.get("providerID").and_then(Value::as_str) == Some(PROVIDER_ID)
                    && candidate.get("modelID").and_then(Value::as_str) == Some(selected_model)
            })
    };
    Ok(selected("user") && selected("recent"))
}

fn desktop_server_state_is_current(data_dir: &Path) -> Result<bool, String> {
    let settings_path = data_dir.join("opencode.settings");
    let global_path = data_dir.join("opencode.global.dat");
    if !settings_path.is_file() || !global_path.is_file() {
        return Ok(false);
    }
    let settings = read_json(&settings_path)?;
    if settings.get("defaultServerUrl").and_then(Value::as_str) != Some(MANAGED_SERVER_URL) {
        return Ok(false);
    }
    let global = read_json(&global_path)?;
    let Some(contents) = global.get("server").and_then(Value::as_str) else {
        return Ok(false);
    };
    let server: Value = serde_json::from_str(contents)
        .map_err(|error| format!("failed to parse OpenCode Desktop server state: {error}"))?;
    Ok(server
        .get("list")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter()
                .any(|entry| server_entry_url(entry) == Some(MANAGED_SERVER_URL))
        }))
}

fn promote_model(
    model_state: &mut serde_json::Map<String, Value>,
    key: &str,
    selected: Value,
    selected_model: &str,
    limit: Option<usize>,
) -> Result<(), String> {
    let values = model_state
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| format!("OpenCode Desktop model state {key} must be an array"))?;
    let existing = values
        .iter()
        .position(|candidate| {
            candidate.get("providerID").and_then(Value::as_str) == Some(PROVIDER_ID)
                && candidate.get("modelID").and_then(Value::as_str) == Some(selected_model)
        })
        .map(|index| values.remove(index));
    let promoted = match (existing, selected.as_object()) {
        (Some(Value::Object(mut existing)), Some(selected)) => {
            existing.extend(selected.clone());
            Value::Object(existing)
        }
        _ => selected,
    };
    values.insert(0, promoted);
    if let Some(limit) = limit {
        values.truncate(limit);
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(value)
        .map_err(|error| format!("failed to serialize {}: {error}", path.display()))?;
    config::atomic_write_text(path, &format!("{contents}\n"))
        .map_err(|error| format!("failed to update {}: {error}", path.display()))
}

fn preserve_backup(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path.with_extension("dat.agentrelay-backup");
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|error| {
            format!(
                "failed to back up OpenCode workspace state to {}: {error}",
                backup.display()
            )
        })?;
    }
    Ok(())
}

fn preserve_named_backup(path: &Path, backup_name: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path
        .parent()
        .ok_or_else(|| format!("invalid OpenCode state path: {}", path.display()))?
        .join(backup_name);
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|error| {
            format!(
                "failed to back up OpenCode Desktop state to {}: {error}",
                backup.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn application_data_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/ai.opencode.desktop"))
        .ok_or_else(|| "cannot locate the OpenCode Desktop data directory".into())
}

#[cfg(windows)]
fn application_data_dir() -> Result<PathBuf, String> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("ai.opencode.desktop"))
        .ok_or_else(|| "cannot locate the OpenCode Desktop data directory".into())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn application_data_dir() -> Result<PathBuf, String> {
    Err("OpenCode Desktop state is supported on macOS and Windows".into())
}

#[cfg(target_os = "macos")]
fn resolve_application() -> Result<PathBuf, String> {
    application_candidates()
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            "OpenCode Desktop is not installed in /Applications or ~/Applications".into()
        })
}

#[cfg(windows)]
fn resolve_application() -> Result<PathBuf, String> {
    application_candidates()
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| "OpenCode Desktop is not installed in a standard location".into())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn resolve_application() -> Result<PathBuf, String> {
    Err("OpenCode Desktop relaunch is supported on macOS and Windows".into())
}

#[cfg(target_os = "macos")]
fn application_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/Applications/OpenCode.app")];
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications/OpenCode.app"));
    }
    candidates
}

#[cfg(windows)]
fn application_candidates() -> Vec<PathBuf> {
    let Some(local_app_data) = env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let root = PathBuf::from(local_app_data);
    vec![
        root.join("Programs/@opencode-aidesktop/OpenCode.exe"),
        root.join("Programs/OpenCode/OpenCode.exe"),
        root.join("OpenCode/OpenCode.exe"),
    ]
}

#[cfg(not(any(target_os = "macos", windows)))]
fn application_candidates() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn is_running(application: &Path) -> Result<bool, String> {
    Ok(!mac_application_processes(application)?.is_empty())
}

#[cfg(windows)]
fn is_running(application: &Path) -> Result<bool, String> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let application = normalize_windows_executable_path(application);
    Ok(system
        .processes()
        .values()
        .any(|process| process_matches_application(process.exe(), &application)))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn is_running(_application: &Path) -> Result<bool, String> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn request_quit(application: &Path) -> Result<(), String> {
    if !is_running(application)? {
        return Ok(());
    }
    let status = Command::new("/usr/bin/osascript")
        .args(["-e", "tell application \"OpenCode\" to quit"])
        .status()
        .map_err(|error| format!("failed to ask OpenCode Desktop to quit: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "OpenCode Desktop rejected the quit request".into())
}

#[cfg(target_os = "macos")]
fn force_quit(application: &Path) -> Result<(), String> {
    if !is_running(application)? {
        return Ok(());
    }
    for pid in mac_application_processes(application)? {
        let status = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .map_err(|error| format!("failed to stop OpenCode Desktop process {pid}: {error}"))?;
        if !status.success() {
            return Err(format!(
                "OpenCode Desktop process {pid} rejected the confirmed restart request"
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn mac_application_processes(application: &Path) -> Result<Vec<u32>, String> {
    let application = fs::canonicalize(application).unwrap_or_else(|_| application.to_path_buf());
    let marker = format!("{}/Contents/", application.to_string_lossy());
    let output = Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .map_err(|error| format!("failed to inspect OpenCode Desktop processes: {error}"))?;
    if !output.status.success() {
        return Err("failed to inspect OpenCode Desktop processes".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let boundary = line.find(char::is_whitespace)?;
            let pid = &line[..boundary];
            let command = &line[boundary..];
            command
                .trim_start()
                .contains(&marker)
                .then(|| pid.parse::<u32>().ok())
                .flatten()
        })
        .collect())
}

#[cfg(windows)]
fn force_quit(application: &Path) -> Result<(), String> {
    let processes = matching_application_processes(application);
    for pid in process_tree_roots(&processes) {
        Command::new(windows_system_tool("taskkill.exe"))
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|error| {
                format!("failed to stop OpenCode Desktop process tree {pid}: {error}")
            })?;
    }
    Ok(())
}

#[cfg(windows)]
fn matching_application_processes(application: &Path) -> Vec<(u32, Option<u32>)> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let application = normalize_windows_executable_path(application);
    system
        .processes()
        .iter()
        .filter(|(_, process)| process_matches_application(process.exe(), &application))
        .map(|(pid, process)| (pid.as_u32(), process.parent().map(|parent| parent.as_u32())))
        .collect()
}

#[cfg(windows)]
fn process_matches_application(executable: Option<&Path>, normalized_application: &str) -> bool {
    executable.is_some_and(|path| {
        normalize_windows_executable_path(path).eq_ignore_ascii_case(normalized_application)
    })
}

#[cfg(windows)]
fn normalize_windows_executable_path(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalize_windows_path_text(&path.to_string_lossy())
}

#[cfg(windows)]
fn normalize_windows_path_text(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = path.strip_prefix("\\\\?\\").unwrap_or(&path);
    path.to_ascii_lowercase()
}

#[cfg(windows)]
fn windows_system_tool(name: &str) -> PathBuf {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32").join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

#[cfg(windows)]
fn windows_powershell() -> PathBuf {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

#[cfg(windows)]
fn surviving_process_description(application: &Path) -> String {
    let survivors = matching_application_processes(application)
        .into_iter()
        .map(|(pid, _)| pid.to_string())
        .collect::<Vec<_>>();
    if survivors.is_empty() {
        "unknown".into()
    } else {
        survivors.join(", ")
    }
}

#[cfg(not(windows))]
fn surviving_process_description(_application: &Path) -> String {
    "one or more OpenCode processes are still running".into()
}

#[cfg(windows)]
fn process_tree_roots(processes: &[(u32, Option<u32>)]) -> Vec<u32> {
    let ids = processes
        .iter()
        .map(|(pid, _)| *pid)
        .collect::<HashSet<_>>();
    let mut roots = processes
        .iter()
        .filter(|(_, parent)| parent.is_none_or(|parent| !ids.contains(&parent)))
        .map(|(pid, _)| *pid)
        .collect::<Vec<_>>();
    roots.sort_unstable();
    roots
}

#[cfg(not(any(target_os = "macos", windows)))]
fn force_quit(_application: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn request_quit(application: &Path) -> Result<(), String> {
    if !is_running(application)? {
        return Ok(());
    }
    let processes = matching_application_processes(application);
    for pid in process_tree_roots(&processes) {
        Command::new(windows_powershell())
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$process = Get-Process -Id ([int]$args[0]) -ErrorAction SilentlyContinue; if ($process) { [void]$process.CloseMainWindow() }",
                &pid.to_string(),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|error| {
                format!("failed to ask OpenCode Desktop process {pid} to quit: {error}")
            })?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn request_quit(_application: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch(application: &PathBuf) -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg(application)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to relaunch OpenCode Desktop: {error}"))
}

#[cfg(windows)]
fn launch(application: &PathBuf) -> Result<(), String> {
    Command::new(application)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to relaunch OpenCode Desktop: {error}"))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn launch(_application: &PathBuf) -> Result<(), String> {
    Err("OpenCode Desktop relaunch is supported on macOS and Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "agentrelay-opencode-desktop-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    fn global_model_state(directory: &Path) -> Value {
        let global = read_json(&directory.join("opencode.global.dat")).unwrap();
        serde_json::from_str(global["model"].as_str().unwrap()).unwrap()
    }

    fn initialize_navigation_storage(directory: &Path) {
        let storage_path = directory.join("Local Storage").join("leveldb");
        fs::create_dir_all(&storage_path).unwrap();
        let options = LevelDbOptions {
            create_if_missing: true,
            ..LevelDbOptions::default()
        };
        let mut storage = DB::open(&storage_path, options).unwrap();
        storage
            .put(
                format!("META:{OPENCODE_RENDERER_ORIGIN}").as_bytes(),
                b"test-origin-metadata",
            )
            .unwrap();
        storage.close().unwrap();
    }

    fn navigation_value(directory: &Path, window_id: &str) -> Vec<u8> {
        let storage_path = directory.join("Local Storage").join("leveldb");
        let options = LevelDbOptions {
            create_if_missing: false,
            ..LevelDbOptions::default()
        };
        let mut storage = DB::open(&storage_path, options).unwrap();
        let value = storage
            .get(&chromium_local_storage_key(
                OPENCODE_RENDERER_ORIGIN,
                &format!("opencode.desktop.window.{window_id}.last-active-url"),
            ))
            .unwrap()
            .to_vec();
        storage.close().unwrap();
        value
    }

    #[test]
    fn application_candidates_are_specific_to_opencode_desktop() {
        let candidates = application_candidates();
        assert!(!candidates.is_empty());
        assert!(candidates.iter().all(|path| {
            let text = path.to_string_lossy().to_ascii_lowercase();
            text.contains("opencode") && !text.contains("agentrelay")
        }));
    }

    #[cfg(windows)]
    #[test]
    fn application_candidates_include_current_windows_install_location() {
        assert!(application_candidates()
            .iter()
            .any(|path| path.ends_with(Path::new("Programs/@opencode-aidesktop/OpenCode.exe"))));
    }

    #[cfg(windows)]
    #[test]
    fn process_tree_roots_exclude_descendants_and_external_parents() {
        let processes = vec![
            (10, None),
            (11, Some(10)),
            (12, Some(11)),
            (20, Some(999)),
            (21, Some(20)),
        ];

        assert_eq!(process_tree_roots(&processes), vec![10, 20]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_application_matching_normalizes_but_stays_path_specific() {
        let application = normalize_windows_path_text(
            r"C:\Users\TestUser\AppData\Local\Programs\@opencode-aidesktop\OpenCode.exe",
        );

        assert!(process_matches_application(
            Some(Path::new(
                r"\\?\c:/users/testuser/appdata/local/programs/@opencode-aidesktop/opencode.exe"
            )),
            &application
        ));
        assert!(!process_matches_application(
            Some(Path::new(r"C:\Tools\OpenCode.exe")),
            &application
        ));
        assert!(!process_matches_application(None, &application));
    }

    #[test]
    fn selects_the_global_model_but_requires_opencode_to_have_launched_once() {
        let directory = temp_directory("never-launched");

        let error = prepare_new_session(&directory, "workstation/ornith").unwrap_err();

        assert!(error.contains("Open OpenCode Desktop once"));
        assert_eq!(
            global_model_state(&directory).pointer("/recent/0/modelID"),
            Some(&Value::String("workstation/ornith".into()))
        );
        assert!(!directory
            .join("opencode.global.dat.agentrelay-backup")
            .exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refuses_to_guess_between_multiple_opencode_windows() {
        let directory = temp_directory("multiple-windows");
        for id in ["first", "second"] {
            fs::write(
                directory.join(format!("opencode.window.{id}.dat")),
                br#"{"tabs":"[]"}"#,
            )
            .unwrap();
        }

        let error = newest_window_state(&directory).unwrap_err();

        assert!(error.contains("multiple saved windows"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replaces_managed_models_and_preserves_unrelated_recent_entries() {
        let mut global = json!({
            "model": json!({
                "user": [
                    {
                        "providerID": "agentrelay",
                        "modelID": "workstation/ornith",
                        "visibility": "hide",
                        "favorite": true
                    }
                ],
                "recent": [
                    { "providerID": "agentrelay", "modelID": "ornith" },
                    { "providerID": "other", "modelID": "one" },
                    { "providerID": "other", "modelID": "two" },
                    { "providerID": "other", "modelID": "three" },
                    { "providerID": "other", "modelID": "four" },
                    { "providerID": "other", "modelID": "five" }
                ],
                "variant": {
                    "agentrelay/workstation/ornith": "high",
                    "other/one": "low"
                }
            })
            .to_string()
        });

        patch_global_model_selection(&mut global, "agentrelay").unwrap();

        let model: Value = serde_json::from_str(global["model"].as_str().unwrap()).unwrap();
        assert_eq!(
            model.pointer("/recent/0/modelID"),
            Some(&Value::String("agentrelay".into()))
        );
        assert_eq!(model["recent"].as_array().unwrap().len(), 5);
        assert_eq!(
            model.pointer("/user/0/providerID"),
            Some(&json!("agentrelay"))
        );
        assert!(model.pointer("/user/0/favorite").is_none());
        assert_eq!(model.pointer("/user/0/visibility"), Some(&json!("show")));
        assert!(model
            .pointer("/variant/agentrelay~1workstation~1ornith")
            .is_none());
        assert_eq!(
            model.pointer("/variant/other~1one"),
            Some(&Value::String("low".into()))
        );
        assert_eq!(
            model["recent"]
                .as_array()
                .unwrap()
                .iter()
                .filter(
                    |candidate| candidate.get("providerID").and_then(Value::as_str)
                        == Some("agentrelay")
                )
                .count(),
            1
        );
    }

    #[test]
    fn recognizes_only_a_clean_virtual_desktop_selection() {
        let directory = temp_directory("virtual-selection");
        fs::create_dir_all(&directory).unwrap();
        let write_state = |user: Value, recent: Value| {
            fs::write(
                directory.join("opencode.global.dat"),
                serde_json::to_vec_pretty(&json!({
                    "model": json!({ "user": user, "recent": recent }).to_string()
                }))
                .unwrap(),
            )
            .unwrap();
        };
        let selected = json!({ "providerID": "agentrelay", "modelID": "agentrelay" });
        write_state(json!([selected.clone()]), json!([selected.clone()]));
        assert!(desktop_model_state_is_current(&directory, "agentrelay").unwrap());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selects_opencodes_new_session_route_from_the_active_draft() {
        let directory = temp_directory("active-draft");
        initialize_navigation_storage(&directory);
        fs::write(
            directory.join("opencode.global.dat"),
            serde_json::to_vec_pretty(&json!({ "command.catalog.v1": "{}" })).unwrap(),
        )
        .unwrap();
        let existing = json!({
            "type": "draft",
            "server": "sidecar",
            "draftID": "draft-existing",
            "directory": "C:\\Users\\brent\\Documents\\Default Project",
            "worktree": "C:\\Users\\brent\\Documents\\Default Project\\tree"
        });
        let window_path = directory.join("opencode.window.current.dat");
        let window_original = serde_json::to_vec_pretty(&json!({
            "tabs": json!([existing]).to_string(),
            "tabs.recent": json!({ "key": "draft:draft-existing" }).to_string()
        }))
        .unwrap();
        fs::write(&window_path, &window_original).unwrap();
        let workspace_path = directory.join("opencode.workspace.test.dat");
        let workspace_original = b"workspace state must remain unchanged";
        fs::write(&workspace_path, workspace_original).unwrap();

        prepare_new_session(&directory, "workstation/ornith").unwrap();

        let patched_window = read_json(&window_path).unwrap();
        let patched_tabs: Vec<Value> =
            serde_json::from_str(patched_window["tabs"].as_str().unwrap()).unwrap();
        assert_eq!(
            patched_tabs[0].get("server").and_then(Value::as_str),
            Some(MANAGED_SERVER_URL)
        );
        assert_eq!(fs::read(&workspace_path).unwrap(), workspace_original);
        assert_eq!(
            navigation_value(&directory, "current"),
            chromium_local_storage_string(
                "/QzpcVXNlcnNcYnJlbnRcRG9jdW1lbnRzXERlZmF1bHQgUHJvamVjdA/session"
            )
        );
        assert!(directory
            .join("Local Storage/leveldb.agentrelay-backup/CURRENT")
            .is_file());
        assert!(directory
            .join("Local Storage/leveldb.agentrelay-previous/CURRENT")
            .is_file());
        assert_eq!(
            read_json(&directory.join("opencode.settings")).unwrap()["defaultServerUrl"],
            Value::String(MANAGED_SERVER_URL.into())
        );
        let global = read_json(&directory.join("opencode.global.dat")).unwrap();
        let server: Value = serde_json::from_str(global["server"].as_str().unwrap()).unwrap();
        assert_eq!(
            server.pointer("/list/0/http/url").and_then(Value::as_str),
            Some(MANAGED_SERVER_URL)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selects_opencodes_new_session_route_from_real_session_tab_state() {
        let directory = temp_directory("active-session");
        initialize_navigation_storage(&directory);
        let recent_key = "sidecar\n/server/c2lkZWNhcg/session/ses_active";
        let window_path = directory.join("opencode.window.current.dat");
        let window_original = serde_json::to_vec_pretty(&json!({
            "tabs": json!([{
                "type": "session",
                "server": "sidecar",
                "sessionId": "ses_active"
            }]).to_string(),
            "tabs.recent": json!({ "key": recent_key }).to_string(),
            "tabs.info": json!({
                recent_key: {
                    "title": "Existing chat",
                    "directory": "C:\\code\\active-project"
                }
            }).to_string()
        }))
        .unwrap();
        fs::write(&window_path, &window_original).unwrap();

        patch_window_servers(&directory).unwrap();
        focus_new_session(&directory, &json!({})).unwrap();

        let window = read_json(&window_path).unwrap();
        let tabs: Vec<Value> = serde_json::from_str(window["tabs"].as_str().unwrap()).unwrap();
        assert_eq!(
            tabs[0].get("server").and_then(Value::as_str),
            Some(MANAGED_SERVER_URL)
        );
        let recent: Value = serde_json::from_str(window["tabs.recent"].as_str().unwrap()).unwrap();
        let expected_key = session_tab_key(MANAGED_SERVER_URL, "ses_active");
        assert_eq!(
            recent.get("key").and_then(Value::as_str),
            Some(expected_key.as_str())
        );
        let info: Value = serde_json::from_str(window["tabs.info"].as_str().unwrap()).unwrap();
        assert_eq!(
            info.pointer(&format!(
                "/{}",
                expected_key.replace('~', "~0").replace('/', "~1")
            ))
            .and_then(|value| value.get("directory"))
            .and_then(Value::as_str),
            Some("C:\\code\\active-project")
        );
        assert_eq!(
            navigation_value(&directory, "current"),
            chromium_local_storage_string("/QzpcY29kZVxhY3RpdmUtcHJvamVjdA/session")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn global_last_project_is_a_safe_directory_fallback() {
        let global = json!({
            "server": json!({
                "lastProject": { "local": "C:\\code\\fallback" }
            })
            .to_string()
        });
        assert_eq!(
            global_last_project(&global).as_deref(),
            Some("C:\\code\\fallback")
        );
    }
}
