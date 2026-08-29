use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use keyring::Entry;

use crate::{
    domain::GatewayRuntimeState,
    fleet::SharedFleetService,
    gateway::{GatewayHeartbeat, SharedGatewayCoordinator},
};

const PHOTON_CREDENTIAL_SERVICE: &str = "com.brent.agentrelay.photon";
const PHOTON_CREDENTIAL_USER: &str = "project-secret";
const SUPERVISOR_INTERVAL: Duration = Duration::from_secs(5);

struct ManagedGateway {
    child: Child,
    fingerprint: String,
    #[cfg(windows)]
    _lifetime_job: std::os::windows::io::OwnedHandle,
}

pub struct GatewaySupervisor {
    config_dir: PathBuf,
    coordinator: SharedGatewayCoordinator,
    fleet: SharedFleetService,
    process: Mutex<Option<ManagedGateway>>,
}

impl GatewaySupervisor {
    pub fn new(
        config_dir: PathBuf,
        coordinator: SharedGatewayCoordinator,
        fleet: SharedFleetService,
    ) -> Self {
        Self {
            config_dir,
            coordinator,
            fleet,
            process: Mutex::new(None),
        }
    }

    pub fn credentials_configured(&self) -> bool {
        photon_secret().is_ok_and(|secret| !secret.is_empty())
    }

    pub fn store_secret(&self, secret: &str) -> Result<(), String> {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err("Photon project secret cannot be empty".into());
        }
        credential_entry()?
            .set_password(secret)
            .map_err(|error| format!("failed to store Photon credentials: {error}"))?;
        self.restart();
        Ok(())
    }

    pub fn clear_secret(&self) -> Result<(), String> {
        match credential_entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                self.restart();
                Ok(())
            }
            Err(error) => Err(format!("failed to remove Photon credentials: {error}")),
        }
    }

    pub fn restart(&self) {
        stop_managed(&mut self.process.lock().expect("gateway process poisoned"));
    }

    pub fn start(self: Arc<Self>) {
        tauri::async_runtime::spawn(async move {
            loop {
                if let Err(error) = self.reconcile() {
                    self.publish(GatewayRuntimeState::Error, Some(error));
                }
                tokio::time::sleep(SUPERVISOR_INTERVAL).await;
            }
        });
    }

    fn reconcile(&self) -> Result<(), String> {
        let config = self.coordinator.config();
        let local_host_id = self.fleet.local_host_id();
        let eligible = config.primary_host_id.as_deref() == Some(local_host_id)
            || config.secondary_host_id.as_deref() == Some(local_host_id);
        if !eligible {
            stop_managed(&mut self.process.lock().expect("gateway process poisoned"));
            self.fleet.update_channel_gateway_status(None);
            return Ok(());
        }

        let project_id = match config.photon_project_id.as_deref() {
            Some(value) if !value.trim().is_empty() => value.trim(),
            _ => {
                stop_managed(&mut self.process.lock().expect("gateway process poisoned"));
                self.publish(
                    GatewayRuntimeState::NeedsCredentials,
                    Some("Photon project id is required".into()),
                );
                return Ok(());
            }
        };
        if config.allowed_senders.is_empty() {
            stop_managed(&mut self.process.lock().expect("gateway process poisoned"));
            self.publish(
                GatewayRuntimeState::NeedsCredentials,
                Some("At least one allowed Photon sender is required".into()),
            );
            return Ok(());
        }
        let secret = match photon_secret() {
            Ok(secret) if !secret.is_empty() => secret,
            _ => {
                stop_managed(&mut self.process.lock().expect("gateway process poisoned"));
                self.publish(
                    GatewayRuntimeState::NeedsCredentials,
                    Some("Photon project secret is required on this machine".into()),
                );
                return Ok(());
            }
        };

        let fingerprint = format!(
            "{}\0{}\0{}",
            project_id,
            config.allowed_senders.join(","),
            self.fleet.proxy_listen_address()
        );
        let mut managed = self.process.lock().expect("gateway process poisoned");
        if let Some(current) = managed.as_mut() {
            if current
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_none()
                && current.fingerprint == fingerprint
            {
                return Ok(());
            }
            stop_managed(&mut managed);
        }

        self.publish(GatewayRuntimeState::Starting, None);
        let executable = sidecar_executable_path()?;
        let mut command = Command::new(&executable);
        command
            .env("PHOTON_PROJECT_ID", project_id)
            .env("PHOTON_PROJECT_SECRET", secret)
            .env(
                "AGENT_RELAY_ALLOWED_SENDERS",
                config.allowed_senders.join(","),
            )
            .env(
                "AGENT_RELAY_ENDPOINT",
                format!("http://{}", self.fleet.proxy_listen_address()),
            )
            .env(
                "AGENT_RELAY_CHECKPOINT_PATH",
                self.config_dir.join("channel-checkpoints.json"),
            )
            .env("AGENT_RELAY_ADAPTER_ID", "photon-imessage")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        #[cfg(windows)]
        let mut child = command.spawn().map_err(|error| {
            format!(
                "failed to start the packaged Photon gateway {}: {error}",
                executable.display()
            )
        })?;
        #[cfg(not(windows))]
        let child = command.spawn().map_err(|error| {
            format!(
                "failed to start the packaged Photon gateway {}: {error}",
                executable.display()
            )
        })?;
        #[cfg(windows)]
        let lifetime_job = match assign_kill_on_close_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        *managed = Some(ManagedGateway {
            child,
            fingerprint,
            #[cfg(windows)]
            _lifetime_job: lifetime_job,
        });
        Ok(())
    }

    fn publish(&self, state: GatewayRuntimeState, error: Option<String>) {
        let status = self
            .coordinator
            .heartbeat(GatewayHeartbeat { state, error });
        self.fleet.update_channel_gateway_status(Some(status));
    }
}

impl Drop for GatewaySupervisor {
    fn drop(&mut self) {
        stop_managed(self.process.get_mut().expect("gateway process poisoned"));
    }
}

pub type SharedGatewaySupervisor = Arc<GatewaySupervisor>;

fn credential_entry() -> Result<Entry, String> {
    Entry::new(PHOTON_CREDENTIAL_SERVICE, PHOTON_CREDENTIAL_USER)
        .map_err(|error| format!("failed to open the operating-system credential store: {error}"))
}

fn photon_secret() -> Result<String, String> {
    credential_entry()?
        .get_password()
        .map_err(|error| format!("Photon credentials are unavailable: {error}"))
}

fn stop_managed(process: &mut Option<ManagedGateway>) {
    if let Some(mut managed) = process.take() {
        let _ = managed.child.kill();
        let _ = managed.child.wait();
    }
}

#[cfg(windows)]
fn assign_kill_on_close_job(child: &Child) -> Result<std::os::windows::io::OwnedHandle, String> {
    use std::{
        mem::{size_of, zeroed},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: Windows owns the returned job handle until `OwnedHandle` drops it. Both
    // information structures and process handles remain valid for the duration of each call.
    unsafe {
        let raw_job = CreateJobObjectW(ptr::null(), ptr::null());
        if raw_job.is_null() {
            return Err(format!(
                "failed to create the Photon gateway lifetime guard: {}",
                std::io::Error::last_os_error()
            ));
        }
        let job = OwnedHandle::from_raw_handle(raw_job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            raw_job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(format!(
                "failed to configure the Photon gateway lifetime guard: {}",
                std::io::Error::last_os_error()
            ));
        }
        if AssignProcessToJobObject(raw_job, child.as_raw_handle()) == 0 {
            return Err(format!(
                "failed to attach the Photon gateway lifetime guard: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(job)
    }
}

fn sidecar_path_from_executable(current: &Path) -> Result<PathBuf, String> {
    let directory = current
        .parent()
        .ok_or_else(|| "Agent Relay executable has no parent directory".to_owned())?;
    let name = if cfg!(windows) {
        "agent-relay-gateway.exe"
    } else {
        "agent-relay-gateway"
    };
    Ok(directory.join(name))
}

fn sidecar_executable_path() -> Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("failed to locate Agent Relay: {error}"))?;
    let sidecar = sidecar_path_from_executable(&current)?;
    if sidecar.is_file() {
        Ok(sidecar)
    } else {
        Err(format!(
            "packaged Photon gateway is missing: {}",
            sidecar.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_gateway_beside_the_app() {
        let executable = if cfg!(windows) {
            Path::new(r"C:\Program Files\Agent Relay\agent-relay.exe")
        } else {
            Path::new("/Applications/Agent Relay.app/Contents/MacOS/agent-relay")
        };
        let resolved = sidecar_path_from_executable(executable).unwrap();
        assert_eq!(
            resolved.file_name().unwrap().to_string_lossy(),
            if cfg!(windows) {
                "agent-relay-gateway.exe"
            } else {
                "agent-relay-gateway"
            }
        );
    }
}
