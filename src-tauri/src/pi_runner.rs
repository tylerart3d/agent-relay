use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    channels::{HarnessDeliveryRequest, HarnessDeliveryResponse},
    fleet::SharedFleetService,
    local_harness::{model_context_window, SharedLocalHarnessIntegrations},
    terminal::{self, CliHarness},
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct PiRunner {
    harnesses: SharedLocalHarnessIntegrations,
    delivery_lock: tokio::sync::Mutex<()>,
}

impl PiRunner {
    pub fn new(harnesses: SharedLocalHarnessIntegrations) -> Self {
        Self {
            harnesses,
            delivery_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn deliver_message(
        &self,
        request: &HarnessDeliveryRequest,
        fleet: &SharedFleetService,
    ) -> Result<HarnessDeliveryResponse, String> {
        let _guard = self.delivery_lock.lock().await;
        let native_session_id = request
            .native_session_id
            .as_deref()
            .ok_or_else(|| "Pi delivery requires native_session_id".to_owned())?;
        let snapshot = fleet.snapshot();
        let selected_model = format!("{}/{}", request.host_id, request.model_id);
        let context_window = model_context_window(&snapshot, &selected_model);
        let status = self.harnesses.connect_pi(
            selected_model.clone(),
            &snapshot.proxy_endpoint,
            context_window,
        )?;
        fleet.update_pi_status(status);

        let project = resolve_project_directory(request.project.as_deref())?;
        let executable = terminal::resolve_executable(CliHarness::Pi)?;
        let agent_dir = self.harnesses.pi_agent_dir()?;
        let mut child = pi_command(
            &executable,
            &project,
            &agent_dir,
            native_session_id,
            &selected_model,
            request.session_id,
        )
        .spawn()
        .map_err(|error| format!("failed to start Pi: {error}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Pi stdin was unavailable".to_owned())?;
        stdin
            .write_all(request.text.as_bytes())
            .await
            .map_err(|error| format!("failed to send prompt to Pi: {error}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| format!("failed to close Pi prompt input: {error}"))?;
        drop(stdin);

        let output = tokio::time::timeout(DELIVERY_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "timed out waiting for Pi after 30 minutes".to_owned())?
            .map_err(|error| format!("failed while waiting for Pi: {error}"))?;
        if output.stdout.len() > MAX_OUTPUT_BYTES || output.stderr.len() > MAX_OUTPUT_BYTES {
            return Err("Pi output exceeded the 32 MiB safety limit".into());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !output.status.success() {
            return Err(if stderr.is_empty() {
                format!("Pi exited with {}", output.status)
            } else {
                format!("Pi failed: {stderr}")
            });
        }
        let reply = parse_pi_json_output(&output.stdout)?;
        Ok(HarnessDeliveryResponse {
            reply,
            native_session_id: Some(native_session_id.to_owned()),
        })
    }

    pub fn set_session_archived(
        &self,
        native_session_id: &str,
        _archived: bool,
    ) -> Result<(), String> {
        if native_session_id.trim().is_empty() {
            return Err("Pi native session ID cannot be empty".into());
        }
        // Pi's noninteractive runner exits after every turn and has no native
        // archive flag. Its JSONL transcript remains available for an explicit
        // Agent Relay resume, while no native process or active conversation is
        // left behind.
        Ok(())
    }
}

pub type SharedPiRunner = Arc<PiRunner>;

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
            "Pi project directory {} does not exist; use an absolute path when the project is not under the harness user's home directory",
            path.display()
        ));
    }
    Ok(path)
}

fn parse_pi_json_output(output: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(output)
        .map_err(|error| format!("Pi returned non-UTF-8 output: {error}"))?;
    let mut reply = None;
    let mut failure = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line)
            .map_err(|error| format!("Pi returned invalid JSON output: {error}"))?;
        if event.get("type").and_then(Value::as_str) != Some("message_end") {
            continue;
        }
        let Some(message) = event.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if message.get("stopReason").and_then(Value::as_str) == Some("error") {
            failure = message
                .get("errorMessage")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some("Pi model turn failed".into()));
            continue;
        }
        let assembled = message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !assembled.is_empty() {
            reply = Some(assembled);
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    reply.ok_or_else(|| "Pi returned no assistant text".into())
}

fn pi_command(
    executable: &Path,
    project: &Path,
    agent_dir: &Path,
    native_session_id: &str,
    selected_model: &str,
    session_id: u64,
) -> Command {
    let arguments = [
        "--mode",
        "json",
        "--print",
        "--session-id",
        native_session_id,
        "--provider",
        "agentrelay",
        "--model",
        selected_model,
        "--name",
        &format!("Agent Relay session #{session_id}"),
    ];
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new(executable);
        command.args(arguments);
        command
    };
    #[cfg(windows)]
    let mut command = {
        fn literal(value: &str) -> String {
            format!("'{}'", value.replace('\'', "''"))
        }
        let script = std::iter::once(format!("& {}", literal(&executable.to_string_lossy())))
            .chain(arguments.iter().map(|argument| literal(argument)))
            .collect::<Vec<_>>()
            .join(" ");
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW);
        command
    };
    command
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .current_dir(project)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_last_successful_assistant_message() {
        let output = br#"{"type":"session","id":"abc"}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"first"}],"stopReason":"stop"}}
{"type":"message_end","message":{"role":"toolResult","content":[{"type":"text","text":"ignored"}]}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"final "},{"type":"text","text":"answer"}],"stopReason":"stop"}}
"#;
        assert_eq!(parse_pi_json_output(output).unwrap(), "final answer");
    }

    #[test]
    fn reports_pi_model_turn_errors_even_when_the_process_exits_zero() {
        let output = br#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"model unavailable"}}
"#;
        assert_eq!(
            parse_pi_json_output(output).unwrap_err(),
            "model unavailable"
        );
    }
}
