use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const ROUTES_FILE_NAME: &str = "channel-routes.json";
const TRANSCRIPTS_FILE_NAME: &str = "channel-transcripts.json";
const MAX_HANDOFF_EXCHANGES: usize = 64;
const MAX_HANDOFF_BYTES: usize = 128 * 1024;
const ADAPTER_STALE_AFTER_MS: u64 = 30_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelAdapterState {
    Connected,
    Error,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChannelAdapterHeartbeat {
    pub adapter_id: String,
    pub channel: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub display_name: String,
    pub state: ChannelAdapterState,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChannelAdapterStatus {
    pub adapter_id: String,
    pub channel: String,
    pub account_id: Option<String>,
    pub display_name: String,
    pub state: ChannelAdapterState,
    pub online: bool,
    pub last_seen_ms: u64,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ChannelAdapterRegistry {
    adapters: RwLock<HashMap<String, ChannelAdapterStatus>>,
}

impl ChannelAdapterRegistry {
    pub fn heartbeat(
        &self,
        heartbeat: ChannelAdapterHeartbeat,
    ) -> Result<ChannelAdapterStatus, String> {
        validate_identifier("adapter_id", &heartbeat.adapter_id)?;
        validate_identifier("channel", &heartbeat.channel)?;
        if let Some(account_id) = heartbeat.account_id.as_deref() {
            validate_identifier("account_id", account_id)?;
        }
        validate_optional_label(Some(&heartbeat.display_name))?;
        let status = ChannelAdapterStatus {
            adapter_id: heartbeat.adapter_id.clone(),
            channel: heartbeat.channel,
            account_id: heartbeat.account_id,
            display_name: heartbeat.display_name,
            online: heartbeat.state == ChannelAdapterState::Connected,
            state: heartbeat.state,
            last_seen_ms: now_ms(),
            error: heartbeat.error,
        };
        self.adapters
            .write()
            .expect("channel adapters poisoned")
            .insert(heartbeat.adapter_id, status.clone());
        Ok(status)
    }

    pub fn list(&self) -> Vec<ChannelAdapterStatus> {
        self.list_at(now_ms())
    }

    fn list_at(&self, timestamp_ms: u64) -> Vec<ChannelAdapterStatus> {
        let mut adapters = self
            .adapters
            .read()
            .expect("channel adapters poisoned")
            .values()
            .cloned()
            .map(|mut adapter| {
                adapter.online = adapter.state == ChannelAdapterState::Connected
                    && timestamp_ms.saturating_sub(adapter.last_seen_ms) <= ADAPTER_STALE_AFTER_MS;
                adapter
            })
            .collect::<Vec<_>>();
        adapters.sort_by(|left, right| left.display_name.cmp(&right.display_name));
        adapters
    }
}

pub type SharedChannelAdapterRegistry = Arc<ChannelAdapterRegistry>;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelHarness {
    Direct,
    Hermes,
    OpenCode,
    Pi,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelHandoffStatus {
    Pending,
    Completed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelNativeArchiveStatus {
    Pending,
    Completed,
    Failed,
}

impl ChannelHarness {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "direct" | "model" => Some(Self::Direct),
            "hermes" => Some(Self::Hermes),
            "opencode" | "open_code" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Direct => "Direct model",
            Self::Hermes => "Hermes",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
        }
    }

    pub fn command_name(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Hermes => "hermes",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChannelAddress {
    pub channel: String,
    #[serde(default = "default_account_id")]
    pub account_id: String,
    pub conversation_id: String,
}

impl ChannelAddress {
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier("channel", &self.channel)?;
        validate_identifier("account_id", &self.account_id)?;
        validate_identifier("conversation_id", &self.conversation_id)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChannelRoute {
    #[serde(flatten)]
    pub address: ChannelAddress,
    #[serde(default)]
    pub session_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_label: Option<String>,
    pub harness: ChannelHarness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_host_id: Option<String>,
    pub host_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_from_session_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_status: Option<ChannelHandoffStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_archive_status: Option<ChannelNativeArchiveStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_archive_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_archived_at_ms: Option<u64>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChannelExchange {
    #[serde(flatten)]
    pub address: ChannelAddress,
    pub session_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_message_id: Option<String>,
    pub user_text: String,
    pub assistant_text: String,
    pub completed_at_ms: u64,
}

#[derive(Clone, Debug)]
pub struct ChannelRouteTarget {
    pub conversation_label: Option<String>,
    pub harness: ChannelHarness,
    pub harness_host_id: Option<String>,
    pub host_id: String,
    pub model_id: String,
    pub project: Option<String>,
}

impl ChannelRouteTarget {
    fn validate(&self) -> Result<(), String> {
        validate_identifier("host_id", &self.host_id)?;
        validate_identifier("model_id", &self.model_id)?;
        if let Some(harness_host_id) = self.harness_host_id.as_deref() {
            validate_identifier("harness_host_id", harness_host_id)?;
        }
        validate_optional_label(self.conversation_label.as_deref())?;
        if self
            .project
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("project cannot be empty".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChannelCommandRequest {
    #[serde(flatten)]
    pub address: ChannelAddress,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub conversation_label: Option<String>,
    #[serde(default)]
    pub external_message_id: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChannelDeliveryRequest {
    #[serde(flatten)]
    pub address: ChannelAddress,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub external_message_id: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessDeliveryRequest {
    pub session_id: u64,
    pub native_session_id: Option<String>,
    pub idempotency_key: String,
    pub host_id: String,
    pub model_id: String,
    pub project: Option<String>,
    pub text: String,
}

impl HarnessDeliveryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.session_id == 0 {
            return Err("session_id must be greater than zero".into());
        }
        if let Some(native_session_id) = self.native_session_id.as_deref() {
            validate_identifier("native_session_id", native_session_id)?;
        }
        validate_identifier("idempotency_key", &self.idempotency_key)?;
        validate_identifier("host_id", &self.host_id)?;
        validate_identifier("model_id", &self.model_id)?;
        if self
            .project
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("project cannot be empty".into());
        }
        if self.text.trim().is_empty() {
            return Err("message text cannot be empty".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessDeliveryResponse {
    pub reply: String,
    pub native_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessSessionArchiveRequest {
    pub native_session_id: String,
    pub archived: bool,
}

impl HarnessSessionArchiveRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier("native_session_id", &self.native_session_id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChannelCommand {
    Help,
    Attach,
    Cancel,
    Status,
    Hosts,
    Models {
        host_id: Option<String>,
    },
    Sessions {
        include_archived: bool,
    },
    Resume {
        session_id: u64,
    },
    Use {
        harness: Option<ChannelHarness>,
        harness_host_id: Option<String>,
        host_id: String,
        model_id: String,
        project: Option<String>,
        native_session_id: Option<String>,
        force: bool,
    },
    New {
        harness: ChannelHarness,
        harness_host_id: Option<String>,
        host_id: String,
        model_id: String,
        project: Option<String>,
        native_session_id: Option<String>,
        force: bool,
    },
    Move {
        harness: ChannelHarness,
        harness_host_id: Option<String>,
        host_id: String,
        model_id: String,
        project: Option<String>,
        native_session_id: Option<String>,
        force: bool,
    },
    Unload {
        host_id: String,
        force: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParsedChannelMessage {
    Message,
    Command(ChannelCommand),
}

pub struct ChannelRouteStore {
    path: PathBuf,
    transcripts_path: PathBuf,
    routes: RwLock<Vec<ChannelRoute>>,
    transcripts: RwLock<Vec<ChannelExchange>>,
}

impl ChannelRouteStore {
    pub fn new(config_dir: &Path) -> Result<Self, String> {
        let path = config_dir.join(ROUTES_FILE_NAME);
        let transcripts_path = config_dir.join(TRANSCRIPTS_FILE_NAME);
        let mut routes: Vec<ChannelRoute> = if path.exists() {
            let contents = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            serde_json::from_str(&contents)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
        } else {
            Vec::new()
        };
        let mut next_id = routes
            .iter()
            .map(|route| route.session_id)
            .max()
            .unwrap_or(0)
            + 1;
        let migrated = routes.iter_mut().fold(false, |migrated, route| {
            if route.session_id == 0 {
                route.session_id = next_id;
                next_id += 1;
                true
            } else {
                migrated
            }
        });
        let store = Self {
            path,
            transcripts: RwLock::new(if transcripts_path.exists() {
                let contents = fs::read_to_string(&transcripts_path).map_err(|error| {
                    format!("failed to read {}: {error}", transcripts_path.display())
                })?;
                serde_json::from_str(&contents).map_err(|error| {
                    format!("failed to parse {}: {error}", transcripts_path.display())
                })?
            } else {
                Vec::new()
            }),
            transcripts_path,
            routes: RwLock::new(routes),
        };
        if migrated {
            store.persist(&store.list())?;
        }
        Ok(store)
    }

    pub fn list(&self) -> Vec<ChannelRoute> {
        self.routes.read().expect("channel routes poisoned").clone()
    }

    pub fn get(&self, address: &ChannelAddress) -> Option<ChannelRoute> {
        self.routes
            .read()
            .expect("channel routes poisoned")
            .iter()
            .find(|route| route.address == *address && route.archived_at_ms.is_none())
            .cloned()
    }

    pub fn sessions(&self, address: &ChannelAddress, include_archived: bool) -> Vec<ChannelRoute> {
        let mut sessions = self
            .routes
            .read()
            .expect("channel routes poisoned")
            .iter()
            .filter(|route| {
                route.address == *address && (include_archived || route.archived_at_ms.is_none())
            })
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|route| std::cmp::Reverse(route.updated_at_ms));
        sessions
    }

    pub fn get_session(&self, address: &ChannelAddress, session_id: u64) -> Option<ChannelRoute> {
        self.routes
            .read()
            .expect("channel routes poisoned")
            .iter()
            .find(|route| route.address == *address && route.session_id == session_id)
            .cloned()
    }

    pub fn set(
        &self,
        address: ChannelAddress,
        target: ChannelRouteTarget,
    ) -> Result<ChannelRoute, String> {
        address.validate()?;
        target.validate()?;
        let ChannelRouteTarget {
            conversation_label,
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
        } = target;

        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        if let Some(existing) = updated
            .iter_mut()
            .find(|existing| existing.address == address && existing.archived_at_ms.is_none())
        {
            let keeps_native_session = existing.harness == harness
                && existing.harness_host_id == harness_host_id
                && existing.project == project;
            if conversation_label.is_some() {
                existing.conversation_label = conversation_label;
            }
            existing.harness = harness;
            existing.harness_host_id = harness_host_id;
            existing.host_id = host_id;
            existing.model_id = model_id;
            existing.project = project;
            if !keeps_native_session {
                existing.native_session_id = None;
            }
            existing.updated_at_ms = now_ms();
        } else {
            updated.push(ChannelRoute {
                address: address.clone(),
                session_id: next_session_id(&updated),
                conversation_label,
                harness,
                harness_host_id,
                host_id,
                model_id,
                project,
                archived_at_ms: None,
                native_session_id: None,
                handoff_from_session_id: None,
                handoff_status: None,
                handoff_completed_at_ms: None,
                native_archive_status: None,
                native_archive_error: None,
                native_archived_at_ms: None,
                updated_at_ms: now_ms(),
            });
        }
        sort_routes(&mut updated);
        let route = updated
            .iter()
            .find(|route| route.address == address && route.archived_at_ms.is_none())
            .expect("active route was just inserted")
            .clone();
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn start_session(
        &self,
        address: ChannelAddress,
        target: ChannelRouteTarget,
    ) -> Result<ChannelRoute, String> {
        address.validate()?;
        target.validate()?;
        let ChannelRouteTarget {
            conversation_label,
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
        } = target;
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let timestamp = now_ms();
        for route in updated
            .iter_mut()
            .filter(|route| route.address == address && route.archived_at_ms.is_none())
        {
            route.archived_at_ms = Some(timestamp);
            route.updated_at_ms = timestamp;
        }
        let inherited_label = conversation_label.or_else(|| {
            updated
                .iter()
                .filter(|route| route.address == address)
                .max_by_key(|route| route.updated_at_ms)
                .and_then(|route| route.conversation_label.clone())
        });
        let route = ChannelRoute {
            address,
            session_id: next_session_id(&updated),
            conversation_label: inherited_label,
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
            archived_at_ms: None,
            native_session_id: None,
            handoff_from_session_id: None,
            handoff_status: None,
            handoff_completed_at_ms: None,
            native_archive_status: None,
            native_archive_error: None,
            native_archived_at_ms: None,
            updated_at_ms: timestamp,
        };
        updated.push(route.clone());
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn move_session(
        &self,
        address: ChannelAddress,
        target: ChannelRouteTarget,
    ) -> Result<ChannelRoute, String> {
        address.validate()?;
        target.validate()?;
        let ChannelRouteTarget {
            conversation_label,
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
        } = target;
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let timestamp = now_ms();
        let source = updated
            .iter()
            .find(|route| route.address == address && route.archived_at_ms.is_none())
            .cloned();
        let source_session_id = source.as_ref().map(|route| route.session_id);
        let native_archive_status = source
            .as_ref()
            .filter(|route| {
                route.native_session_id.is_some() && route.harness != ChannelHarness::Direct
            })
            .map(|_| ChannelNativeArchiveStatus::Pending);
        for route in updated
            .iter_mut()
            .filter(|route| route.address == address && route.archived_at_ms.is_none())
        {
            route.archived_at_ms = Some(timestamp);
            route.updated_at_ms = timestamp;
        }
        let inherited_label = conversation_label.or_else(|| {
            updated
                .iter()
                .filter(|route| route.address == address)
                .max_by_key(|route| route.updated_at_ms)
                .and_then(|route| route.conversation_label.clone())
        });
        let route = ChannelRoute {
            address,
            session_id: next_session_id(&updated),
            conversation_label: inherited_label,
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
            archived_at_ms: None,
            native_session_id: None,
            handoff_from_session_id: source_session_id,
            handoff_status: source_session_id.map(|_| ChannelHandoffStatus::Pending),
            handoff_completed_at_ms: None,
            native_archive_status,
            native_archive_error: None,
            native_archived_at_ms: None,
            updated_at_ms: timestamp,
        };
        updated.push(route.clone());
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn resume(
        &self,
        address: &ChannelAddress,
        session_id: u64,
    ) -> Result<ChannelRoute, String> {
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let timestamp = now_ms();
        let Some(target_index) = updated
            .iter()
            .position(|route| route.address == *address && route.session_id == session_id)
        else {
            return Err(format!(
                "session #{session_id} was not found for this conversation"
            ));
        };
        for route in updated
            .iter_mut()
            .filter(|route| route.address == *address && route.archived_at_ms.is_none())
        {
            route.archived_at_ms = Some(timestamp);
            route.updated_at_ms = timestamp;
        }
        updated[target_index].archived_at_ms = None;
        updated[target_index].updated_at_ms = timestamp;
        let route = updated[target_index].clone();
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn bind_native_session(
        &self,
        address: &ChannelAddress,
        session_id: u64,
        native_session_id: String,
    ) -> Result<ChannelRoute, String> {
        if native_session_id.trim().is_empty() {
            return Err("native_session_id cannot be empty".into());
        }
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let Some(route) = updated.iter_mut().find(|route| {
            route.address == *address
                && route.session_id == session_id
                && route.archived_at_ms.is_none()
        }) else {
            return Err(format!("active session #{session_id} was not found"));
        };
        route.native_session_id = Some(native_session_id);
        route.updated_at_ms = now_ms();
        let route = route.clone();
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn attach_existing_native_session(
        &self,
        address: &ChannelAddress,
        session_id: u64,
        native_session_id: String,
    ) -> Result<ChannelRoute, String> {
        if native_session_id.trim().is_empty() {
            return Err("native_session_id cannot be empty".into());
        }
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let Some(route) = updated.iter_mut().find(|route| {
            route.address == *address
                && route.session_id == session_id
                && route.archived_at_ms.is_none()
        }) else {
            return Err(format!("active session #{session_id} was not found"));
        };
        route.native_session_id = Some(native_session_id);
        route.handoff_from_session_id = None;
        route.handoff_status = None;
        route.handoff_completed_at_ms = None;
        route.native_archive_status = None;
        route.native_archive_error = None;
        route.updated_at_ms = now_ms();
        let route = route.clone();
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    #[cfg(test)]
    pub fn exchanges(&self, address: &ChannelAddress, session_id: u64) -> Vec<ChannelExchange> {
        self.transcripts
            .read()
            .expect("channel transcripts poisoned")
            .iter()
            .filter(|exchange| exchange.address == *address && exchange.session_id == session_id)
            .cloned()
            .collect()
    }

    pub fn cached_exchange(
        &self,
        address: &ChannelAddress,
        session_id: u64,
        external_message_id: Option<&str>,
    ) -> Option<ChannelExchange> {
        let external_message_id = external_message_id?;
        self.transcripts
            .read()
            .expect("channel transcripts poisoned")
            .iter()
            .find(|exchange| {
                exchange.address == *address
                    && exchange.session_id == session_id
                    && exchange.external_message_id.as_deref() == Some(external_message_id)
            })
            .cloned()
    }

    pub fn record_exchange(
        &self,
        route: &ChannelRoute,
        external_message_id: Option<String>,
        user_text: String,
        assistant_text: String,
    ) -> Result<ChannelExchange, String> {
        if user_text.trim().is_empty() || assistant_text.trim().is_empty() {
            return Err("channel transcript messages cannot be empty".into());
        }
        if let Some(existing) = self.cached_exchange(
            &route.address,
            route.session_id,
            external_message_id.as_deref(),
        ) {
            return Ok(existing);
        }
        let exchange = ChannelExchange {
            address: route.address.clone(),
            session_id: route.session_id,
            external_message_id,
            user_text,
            assistant_text,
            completed_at_ms: now_ms(),
        };
        let mut transcripts = self
            .transcripts
            .write()
            .expect("channel transcripts poisoned");
        let mut updated = transcripts.clone();
        updated.push(exchange.clone());
        self.persist_transcripts(&updated)?;
        *transcripts = updated;
        Ok(exchange)
    }

    pub fn delivery_text(&self, route: &ChannelRoute, current_text: &str) -> String {
        if route.handoff_status != Some(ChannelHandoffStatus::Pending) {
            return current_text.to_owned();
        }
        let Some(source_session_id) = route.handoff_from_session_id else {
            return current_text.to_owned();
        };
        let exchanges = self.portable_exchanges(&route.address, source_session_id);
        render_handoff_prompt(&exchanges, current_text)
    }

    fn portable_exchanges(
        &self,
        address: &ChannelAddress,
        session_id: u64,
    ) -> Vec<ChannelExchange> {
        let routes = self.routes.read().expect("channel routes poisoned");
        let mut lineage = Vec::new();
        let mut next = Some(session_id);
        let mut visited = HashSet::new();
        while let Some(candidate) = next {
            if !visited.insert(candidate) {
                break;
            }
            lineage.push(candidate);
            next = routes
                .iter()
                .find(|route| route.address == *address && route.session_id == candidate)
                .and_then(|route| route.handoff_from_session_id);
        }
        lineage.reverse();
        drop(routes);

        let transcripts = self
            .transcripts
            .read()
            .expect("channel transcripts poisoned");
        lineage
            .into_iter()
            .flat_map(|candidate| {
                transcripts
                    .iter()
                    .filter(move |exchange| {
                        exchange.address == *address && exchange.session_id == candidate
                    })
                    .cloned()
            })
            .collect()
    }

    pub fn complete_handoff(
        &self,
        address: &ChannelAddress,
        session_id: u64,
    ) -> Result<ChannelRoute, String> {
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let Some(route) = updated.iter_mut().find(|route| {
            route.address == *address
                && route.session_id == session_id
                && route.archived_at_ms.is_none()
        }) else {
            return Err(format!("active session #{session_id} was not found"));
        };
        if route.handoff_status == Some(ChannelHandoffStatus::Pending) {
            let timestamp = now_ms();
            route.handoff_status = Some(ChannelHandoffStatus::Completed);
            route.handoff_completed_at_ms = Some(timestamp);
            route.updated_at_ms = timestamp;
        }
        let route = route.clone();
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn record_native_archive_result(
        &self,
        address: &ChannelAddress,
        destination_session_id: u64,
        source_session_id: u64,
        result: Result<(), String>,
    ) -> Result<ChannelRoute, String> {
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let timestamp = now_ms();
        let Some(destination_index) = updated.iter().position(|route| {
            route.address == *address && route.session_id == destination_session_id
        }) else {
            return Err(format!(
                "destination session #{destination_session_id} was not found"
            ));
        };
        let Some(source_index) = updated
            .iter()
            .position(|route| route.address == *address && route.session_id == source_session_id)
        else {
            return Err(format!("source session #{source_session_id} was not found"));
        };
        match result {
            Ok(()) => {
                updated[destination_index].native_archive_status =
                    Some(ChannelNativeArchiveStatus::Completed);
                updated[destination_index].native_archive_error = None;
                updated[source_index].native_archived_at_ms = Some(timestamp);
            }
            Err(error) => {
                updated[destination_index].native_archive_status =
                    Some(ChannelNativeArchiveStatus::Failed);
                updated[destination_index].native_archive_error = Some(error);
            }
        }
        updated[destination_index].updated_at_ms = timestamp;
        let route = updated[destination_index].clone();
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    pub fn mark_native_unarchived(
        &self,
        address: &ChannelAddress,
        session_id: u64,
    ) -> Result<ChannelRoute, String> {
        let mut routes = self.routes.write().expect("channel routes poisoned");
        let mut updated = routes.clone();
        let Some(route) = updated
            .iter_mut()
            .find(|route| route.address == *address && route.session_id == session_id)
        else {
            return Err(format!("session #{session_id} was not found"));
        };
        route.native_archived_at_ms = None;
        route.updated_at_ms = now_ms();
        let route = route.clone();
        sort_routes(&mut updated);
        self.persist(&updated)?;
        *routes = updated;
        Ok(route)
    }

    fn persist(&self, routes: &[ChannelRoute]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create channel route directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let contents = serde_json::to_string_pretty(routes)
            .map_err(|error| format!("failed to serialize channel routes: {error}"))?;
        crate::config::atomic_write_text(&self.path, &contents)
    }

    fn persist_transcripts(&self, transcripts: &[ChannelExchange]) -> Result<(), String> {
        if let Some(parent) = self.transcripts_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create channel transcript directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let contents = serde_json::to_string_pretty(transcripts)
            .map_err(|error| format!("failed to serialize channel transcripts: {error}"))?;
        crate::config::atomic_write_text(&self.transcripts_path, &contents)
    }
}

fn render_handoff_prompt(exchanges: &[ChannelExchange], current_text: &str) -> String {
    if exchanges.is_empty() {
        return current_text.to_owned();
    }
    let mut selected: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for exchange in exchanges.iter().rev().take(MAX_HANDOFF_EXCHANGES) {
        let encoded = serde_json::to_string(&serde_json::json!({
            "user": exchange.user_text,
            "assistant": exchange.assistant_text,
        }))
        .expect("channel transcript values always serialize");
        let encoded_bytes = encoded.len().saturating_add(2);
        if bytes.saturating_add(encoded_bytes) > MAX_HANDOFF_BYTES {
            if selected.is_empty() {
                continue;
            }
            break;
        }
        bytes = bytes.saturating_add(encoded_bytes);
        selected.push(encoded);
    }
    selected.reverse();
    let omitted = exchanges.len().saturating_sub(selected.len());
    let transcript = format!("[\n{}\n]", selected.join(",\n"));
    format!(
        "[Agent Relay conversation handoff]\nYou are continuing a conversation that was previously handled by another harness. Treat the JSON transcript as prior user/assistant conversation context. Do not describe the handoff unless the user asks. {omitted} older exchange(s) were omitted.\n\n{transcript}\n\n[Current user message]\n{current_text}"
    )
}

fn next_session_id(routes: &[ChannelRoute]) -> u64 {
    routes
        .iter()
        .map(|route| route.session_id)
        .max()
        .unwrap_or(0)
        + 1
}

fn sort_routes(routes: &mut [ChannelRoute]) {
    routes.sort_by(|left, right| {
        (
            &left.address.channel,
            &left.address.account_id,
            &left.address.conversation_id,
            left.session_id,
        )
            .cmp(&(
                &right.address.channel,
                &right.address.account_id,
                &right.address.conversation_id,
                right.session_id,
            ))
    });
}

pub type SharedChannelRouteStore = Arc<ChannelRouteStore>;

pub fn parse_channel_message(text: &str) -> Result<ParsedChannelMessage, String> {
    let tokens = tokenize(text)?;
    let Some(prefix) = tokens.first() else {
        return Ok(ParsedChannelMessage::Message);
    };
    if !matches!(
        prefix.to_ascii_lowercase().as_str(),
        "!ar" | "!agentrelay" | "/ar" | "/agentrelay"
    ) {
        return Ok(ParsedChannelMessage::Message);
    }

    let args = &tokens[1..];
    let command = match args.first().map(|value| value.to_ascii_lowercase()) {
        None => ChannelCommand::Help,
        Some(name) if matches!(name.as_str(), "help" | "?") => {
            no_extra(args, ChannelCommand::Help)?
        }
        Some(name) if matches!(name.as_str(), "attach" | "recent") => {
            no_extra(args, ChannelCommand::Attach)?
        }
        Some(name) if name == "cancel" => no_extra(args, ChannelCommand::Cancel)?,
        Some(name) if matches!(name.as_str(), "status" | "route") => {
            no_extra(args, ChannelCommand::Status)?
        }
        Some(name) if name == "hosts" => no_extra(args, ChannelCommand::Hosts)?,
        Some(name) if name == "models" => {
            if args.len() > 2 {
                return Err("usage: !ar models [host]".into());
            }
            ChannelCommand::Models {
                host_id: args.get(1).cloned(),
            }
        }
        Some(name) if name == "sessions" => {
            let include_archived = match args.get(1).map(|value| value.to_ascii_lowercase()) {
                None => true,
                Some(value) if value == "all" => true,
                Some(value) if value == "active" => false,
                Some(value) => return Err(format!("unexpected sessions argument '{value}'")),
            };
            if args.len() > 2 {
                return Err("usage: !ar sessions [all|active]".into());
            }
            ChannelCommand::Sessions { include_archived }
        }
        Some(name) if name == "resume" => parse_resume(&args[1..])?,
        Some(name) if name == "use" => parse_route_command(&args[1..], RouteCommandKind::Use)?,
        Some(name) if name == "new" => parse_route_command(&args[1..], RouteCommandKind::New)?,
        Some(name) if name == "move" => parse_route_command(&args[1..], RouteCommandKind::Move)?,
        Some(name) if matches!(name.as_str(), "add-to" | "add_to") => parse_add_to(&args[1..])?,
        Some(name) if name == "unload" => parse_unload(&args[1..])?,
        Some(name) => {
            return Err(format!(
                "unknown Agent Relay command '{name}'; use !ar help"
            ))
        }
    };
    Ok(ParsedChannelMessage::Command(command))
}

fn no_extra(args: &[String], command: ChannelCommand) -> Result<ChannelCommand, String> {
    if args.len() == 1 {
        Ok(command)
    } else {
        Err(format!("unexpected argument '{}'; use !ar help", args[1]))
    }
}

#[derive(Clone, Copy)]
enum RouteCommandKind {
    Use,
    New,
    Move,
}

fn parse_route_command(args: &[String], kind: RouteCommandKind) -> Result<ChannelCommand, String> {
    if args.is_empty() {
        return Err(route_usage(kind).into());
    }
    let first_harness = parse_harness_target(&args[0]);
    let second_harness = args.get(1).and_then(|value| parse_harness_target(value));
    let (harness, harness_host_id, target, mut position) = match (first_harness, second_harness) {
        (Some((harness, host)), _) => {
            let target = args.get(1).ok_or_else(|| route_usage(kind).to_owned())?;
            (Some(harness), host, target, 2)
        }
        (None, Some((harness, host))) => (Some(harness), host, &args[0], 2),
        (None, None) if matches!(kind, RouteCommandKind::Use) => (None, None, &args[0], 1),
        _ => return Err("harness must be direct, hermes, opencode, or pi".into()),
    };
    let (host_id, model_id) = target
        .split_once('/')
        .filter(|(host, model)| !host.is_empty() && !model.is_empty())
        .ok_or_else(|| "model target must use <host>/<model>".to_owned())?;
    let mut project = None;
    let mut native_session_id = None;
    let mut force = false;
    while position < args.len() {
        match args[position].to_ascii_lowercase().as_str() {
            "project" | "--project" => {
                position += 1;
                project = Some(
                    args.get(position)
                        .cloned()
                        .ok_or_else(|| "project requires a path or project ID".to_owned())?,
                );
            }
            "session" | "--session" => {
                position += 1;
                native_session_id = Some(
                    args.get(position)
                        .cloned()
                        .ok_or_else(|| "session requires an OpenCode session ID".to_owned())?,
                );
            }
            "force" | "--force" if !force => force = true,
            other => return Err(format!("unexpected use argument '{other}'")),
        }
        position += 1;
    }
    if project.is_some()
        && harness.as_ref().is_some_and(|harness| {
            !matches!(harness, ChannelHarness::OpenCode | ChannelHarness::Pi)
        })
    {
        return Err("project is supported only for OpenCode and Pi routes".into());
    }
    if native_session_id.is_some() && harness.as_ref() != Some(&ChannelHarness::OpenCode) {
        return Err("session attachment is supported only for OpenCode routes".into());
    }
    let host_id = host_id.to_owned();
    let model_id = model_id.to_owned();
    match kind {
        RouteCommandKind::Use => Ok(ChannelCommand::Use {
            harness,
            harness_host_id,
            host_id,
            model_id,
            project,
            native_session_id,
            force,
        }),
        RouteCommandKind::New => Ok(ChannelCommand::New {
            harness: harness.expect("new requires a harness"),
            harness_host_id,
            host_id,
            model_id,
            project,
            native_session_id,
            force,
        }),
        RouteCommandKind::Move => Ok(ChannelCommand::Move {
            harness: harness.expect("move requires a harness"),
            harness_host_id,
            host_id,
            model_id,
            project,
            native_session_id,
            force,
        }),
    }
}

fn parse_add_to(args: &[String]) -> Result<ChannelCommand, String> {
    let project = args.first().cloned().ok_or_else(|| {
        "usage: !ar add-to <project> <opencode[@host]> <host>/<model> [force]".to_owned()
    })?;
    let mut route_args = args[1..].to_vec();
    route_args.push("project".into());
    route_args.push(project);
    parse_route_command(&route_args, RouteCommandKind::Move)
}

fn parse_harness_target(value: &str) -> Option<(ChannelHarness, Option<String>)> {
    let (harness, host) = match value.split_once('@') {
        Some((_, "")) => return None,
        Some((harness, host)) => (harness, Some(host)),
        None => (value, None),
    };
    let harness = ChannelHarness::parse(harness)?;
    let host = host.map(str::to_owned);
    Some((harness, host))
}

fn route_usage(kind: RouteCommandKind) -> &'static str {
    match kind {
        RouteCommandKind::Use => {
            "usage: !ar use [harness[@host]] <host>/<model> [project <path>] [session <OpenCode-session-id>] [force]"
        }
        RouteCommandKind::New => {
            "usage: !ar new <harness[@host]> <host>/<model> [project <path>] [session <OpenCode-session-id>] [force]"
        }
        RouteCommandKind::Move => {
            "usage: !ar move <harness[@host]> <host>/<model> [project <path>] [session <OpenCode-session-id>] [force]"
        }
    }
}

fn parse_resume(args: &[String]) -> Result<ChannelCommand, String> {
    if args.len() != 1 {
        return Err("usage: !ar resume <session-number>".into());
    }
    let value = args[0].trim_start_matches('#');
    let session_id = value
        .parse::<u64>()
        .map_err(|_| "session number must be a positive integer".to_owned())?;
    if session_id == 0 {
        return Err("session number must be a positive integer".into());
    }
    Ok(ChannelCommand::Resume { session_id })
}

fn parse_unload(args: &[String]) -> Result<ChannelCommand, String> {
    let host_id = args
        .first()
        .cloned()
        .ok_or_else(|| "usage: !ar unload <host> [force]".to_owned())?;
    let mut force = false;
    for argument in &args[1..] {
        match argument.to_ascii_lowercase().as_str() {
            "force" | "--force" if !force => force = true,
            other => return Err(format!("unexpected unload argument '{other}'")),
        }
    }
    Ok(ChannelCommand::Unload { host_id, force })
}

fn tokenize(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut characters = input.trim().chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let escapes_next = characters.peek().is_some_and(|next| match quote {
                Some(expected) => *next == expected,
                None => next.is_whitespace() || matches!(*next, '\'' | '"'),
            });
            if escapes_next {
                current.push(characters.next().expect("peeked character exists"));
            } else {
                current.push(character);
            }
            continue;
        }
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if quote.is_some() {
        return Err("unterminated quote in Agent Relay command".into());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn validate_identifier(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{name} cannot be empty"))
    } else if value.len() > 512 {
        Err(format!("{name} is too long"))
    } else {
        Ok(())
    }
}

fn validate_optional_label(value: Option<&str>) -> Result<(), String> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        Err("conversation_label cannot be empty".into())
    } else if value.is_some_and(|value| value.len() > 256) {
        Err("conversation_label is too long".into())
    } else {
        Ok(())
    }
}

fn default_account_id() -> String {
    "default".into()
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
    fn adapter_heartbeats_expire_without_removing_the_adapter() {
        let registry = ChannelAdapterRegistry::default();
        let connected = registry
            .heartbeat(ChannelAdapterHeartbeat {
                adapter_id: "photon-imessage".into(),
                channel: "imessage".into(),
                account_id: Some("+15551234567".into()),
                display_name: "Photon iMessage".into(),
                state: ChannelAdapterState::Connected,
                error: None,
            })
            .unwrap();
        assert!(connected.online);
        let stale = registry.list_at(connected.last_seen_ms + ADAPTER_STALE_AFTER_MS + 1);
        assert_eq!(stale.len(), 1);
        assert!(!stale[0].online);
    }

    #[test]
    fn ordinary_messages_are_not_claimed_as_commands() {
        assert_eq!(
            parse_channel_message("hello Agent Relay").unwrap(),
            ParsedChannelMessage::Message
        );
    }

    #[test]
    fn parses_use_in_both_harness_positions() {
        let expected = ParsedChannelMessage::Command(ChannelCommand::Use {
            harness: Some(ChannelHarness::Hermes),
            harness_host_id: None,
            host_id: "workstation".into(),
            model_id: "qwen3.8-mtp-q4".into(),
            project: None,
            native_session_id: None,
            force: false,
        });
        assert_eq!(
            parse_channel_message("/ar use hermes workstation/qwen3.8-mtp-q4").unwrap(),
            expected
        );
        assert_eq!(
            parse_channel_message("/agentrelay use workstation/qwen3.8-mtp-q4 Hermes").unwrap(),
            expected
        );
    }

    #[test]
    fn parses_quoted_opencode_project_and_force() {
        assert_eq!(
            parse_channel_message(
                "/ar use opencode m1-pro/ornith project '/Users/tester/My Project' force"
            )
            .unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Use {
                harness: Some(ChannelHarness::OpenCode),
                harness_host_id: None,
                host_id: "m1-pro".into(),
                model_id: "ornith".into(),
                project: Some("/Users/tester/My Project".into()),
                native_session_id: None,
                force: true,
            })
        );
    }

    #[test]
    fn parses_remote_pi_project_route() {
        assert_eq!(
            parse_channel_message(
                "/ar new pi@m1-pro workstation/qwen project '/Users/tester/Code Lab'"
            )
            .unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::New {
                harness: ChannelHarness::Pi,
                harness_host_id: Some("m1-pro".into()),
                host_id: "workstation".into(),
                model_id: "qwen".into(),
                project: Some("/Users/tester/Code Lab".into()),
                native_session_id: None,
                force: false,
            })
        );
    }

    #[test]
    fn preserves_windows_project_paths() {
        assert_eq!(
            parse_channel_message(
                r#"/ar move pi@workstation m1-pro/qwen project "P:\projects-code\My Project""#,
            )
            .unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Move {
                harness: ChannelHarness::Pi,
                harness_host_id: Some("workstation".into()),
                host_id: "m1-pro".into(),
                model_id: "qwen".into(),
                project: Some(r"P:\projects-code\My Project".into()),
                native_session_id: None,
                force: false,
            })
        );
    }

    #[test]
    fn parses_model_only_use_and_remote_harness_move() {
        assert_eq!(
            parse_channel_message("/ar use workstation/qwen").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Use {
                harness: None,
                harness_host_id: None,
                host_id: "workstation".into(),
                model_id: "qwen".into(),
                project: None,
                native_session_id: None,
                force: false,
            })
        );
        assert_eq!(
            parse_channel_message(
                "/ar move opencode@workstation m1-pro/ornith project agent-relay"
            )
            .unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Move {
                harness: ChannelHarness::OpenCode,
                harness_host_id: Some("workstation".into()),
                host_id: "m1-pro".into(),
                model_id: "ornith".into(),
                project: Some("agent-relay".into()),
                native_session_id: None,
                force: false,
            })
        );
    }

    #[test]
    fn parses_an_existing_opencode_session_attachment() {
        assert_eq!(
            parse_channel_message(
                    "!ar use opencode@m1-pro workstation/qwen project '/Users/tester/Game' session ses_game"
            )
            .unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Use {
                harness: Some(ChannelHarness::OpenCode),
                harness_host_id: Some("m1-pro".into()),
                host_id: "workstation".into(),
                model_id: "qwen".into(),
                project: Some("/Users/tester/Game".into()),
                native_session_id: Some("ses_game".into()),
                force: false,
            })
        );
    }

    #[test]
    fn parses_session_commands() {
        assert_eq!(
            parse_channel_message("/ar sessions all").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Sessions {
                include_archived: true,
            })
        );
        assert_eq!(
            parse_channel_message("/ar resume #12").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Resume { session_id: 12 })
        );
    }

    #[test]
    fn parses_transport_safe_bang_commands() {
        assert_eq!(
            parse_channel_message("!ar status").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Status)
        );
        assert_eq!(
            parse_channel_message("!agentrelay hosts").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Hosts)
        );
        assert_eq!(
            parse_channel_message("!ar attach").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Attach)
        );
        assert_eq!(
            parse_channel_message("!ar recent").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Attach)
        );
        assert_eq!(
            parse_channel_message("!ar route").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Status)
        );
        assert_eq!(
            parse_channel_message("!ar cancel").unwrap(),
            ParsedChannelMessage::Command(ChannelCommand::Cancel)
        );
    }

    #[test]
    fn rejects_projects_for_non_project_harnesses() {
        assert!(
            parse_channel_message("/ar use hermes workstation/qwen project agent-relay")
                .unwrap_err()
                .contains("OpenCode")
        );
    }

    #[test]
    fn rejects_existing_session_attachment_for_non_opencode_harnesses() {
        assert!(
            parse_channel_message("/ar move hermes workstation/qwen session ses_game")
                .unwrap_err()
                .contains("OpenCode")
        );
        assert!(parse_channel_message(
            "/ar move pi workstation/qwen project game session ses_game"
        )
        .unwrap_err()
        .contains("OpenCode"));
    }

    #[test]
    fn persists_one_sticky_route_per_conversation() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-channel-routes-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let address = ChannelAddress {
            channel: "photon".into(),
            account_id: "personal".into(),
            conversation_id: "chat-1".into(),
        };
        let store = ChannelRouteStore::new(&directory).expect("create route store");
        store
            .set(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: Some("Planning chat".into()),
                    harness: ChannelHarness::Hermes,
                    harness_host_id: None,
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: None,
                },
            )
            .expect("set first route");
        store
            .set(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::OpenCode,
                    harness_host_id: Some("workstation".into()),
                    host_id: "m1-pro".into(),
                    model_id: "ornith".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("replace route");

        let reloaded = ChannelRouteStore::new(&directory).expect("reload route store");
        assert_eq!(reloaded.list().len(), 1);
        let route = reloaded.get(&address).expect("saved route");
        assert_eq!(route.harness, ChannelHarness::OpenCode);
        assert_eq!(route.conversation_label.as_deref(), Some("Planning chat"));
        assert_eq!(route.harness_host_id.as_deref(), Some("workstation"));
        assert_eq!(route.host_id, "m1-pro");
        assert_eq!(route.project.as_deref(), Some("agent-relay"));
        fs::remove_dir_all(directory).expect("remove route test directory");
    }

    #[test]
    fn binds_a_native_session_only_to_the_active_agent_relay_session() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-native-session-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let address = ChannelAddress {
            channel: "imessage".into(),
            account_id: "personal".into(),
            conversation_id: "chat-native".into(),
        };
        let store = ChannelRouteStore::new(&directory).expect("create route store");
        let route = store
            .start_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Hermes,
                    harness_host_id: Some("m1-pro".into()),
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: None,
                },
            )
            .expect("start session");
        let bound = store
            .bind_native_session(&address, route.session_id, "agent-relay-session-1".into())
            .expect("bind native session");
        assert_eq!(
            bound.native_session_id.as_deref(),
            Some("agent-relay-session-1")
        );
        let rerouted_model = store
            .set(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Hermes,
                    harness_host_id: Some("m1-pro".into()),
                    host_id: "air-m4".into(),
                    model_id: "gemma".into(),
                    project: None,
                },
            )
            .expect("reroute the same Hermes conversation");
        assert_eq!(
            rerouted_model.native_session_id.as_deref(),
            Some("agent-relay-session-1")
        );
        assert_eq!(
            ChannelRouteStore::new(&directory)
                .unwrap()
                .get(&address)
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("agent-relay-session-1")
        );
        let moved_harness = store
            .set(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::OpenCode,
                    harness_host_id: Some("workstation".into()),
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("change harness");
        assert!(moved_harness.native_session_id.is_none());

        let pi_route = store
            .set(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Pi,
                    harness_host_id: Some("m1-pro".into()),
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("move to Pi");
        let pi_bound = store
            .bind_native_session(&address, pi_route.session_id, "pi-session-id".into())
            .expect("bind Pi session");
        let pi_rerouted = store
            .set(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Pi,
                    harness_host_id: Some("m1-pro".into()),
                    host_id: "air-m4".into(),
                    model_id: "gemma".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("reroute Pi model");
        assert_eq!(
            pi_rerouted.native_session_id.as_deref(),
            pi_bound.native_session_id.as_deref()
        );
        fs::remove_dir_all(directory).expect("remove native session test directory");
    }

    #[test]
    fn attaching_an_existing_session_cancels_portable_context_handoff() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-existing-session-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let address = ChannelAddress {
            channel: "imessage".into(),
            account_id: "personal".into(),
            conversation_id: "game-chat".into(),
        };
        let store = ChannelRouteStore::new(&directory).expect("create route store");
        let first = store
            .start_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Hermes,
                    harness_host_id: None,
                    host_id: "m1-pro".into(),
                    model_id: "ornith".into(),
                    project: None,
                },
            )
            .expect("start source session");
        store
            .record_exchange(&first, None, "idea".into(), "response".into())
            .expect("record source exchange");
        let moved = store
            .move_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::OpenCode,
                    harness_host_id: Some("workstation".into()),
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: Some("P:/game".into()),
                },
            )
            .expect("create handoff route");
        assert_eq!(moved.handoff_status, Some(ChannelHandoffStatus::Pending));

        let attached = store
            .attach_existing_native_session(&address, moved.session_id, "ses_existing".into())
            .expect("attach existing conversation");
        assert_eq!(attached.native_session_id.as_deref(), Some("ses_existing"));
        assert!(attached.handoff_from_session_id.is_none());
        assert!(attached.handoff_status.is_none());
        assert!(attached.handoff_completed_at_ms.is_none());

        fs::remove_dir_all(directory).expect("remove route test directory");
    }

    #[test]
    fn moves_a_hermes_transcript_into_a_pi_project_once() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-portable-handoff-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let address = ChannelAddress {
            channel: "photon".into(),
            account_id: "personal".into(),
            conversation_id: "brainstorm".into(),
        };
        let store = ChannelRouteStore::new(&directory).expect("create route store");
        let hermes = store
            .start_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: Some("Feature idea".into()),
                    harness: ChannelHarness::Hermes,
                    harness_host_id: Some("m1-pro".into()),
                    host_id: "m1-pro".into(),
                    model_id: "ornith".into(),
                    project: None,
                },
            )
            .expect("start Hermes session");
        let hermes = store
            .bind_native_session(&address, hermes.session_id, "agent-relay-source".into())
            .expect("bind Hermes native session");
        store
            .record_exchange(
                &hermes,
                Some("message-1".into()),
                "Let's build a portable handoff.".into(),
                "Use a durable transcript and explicit state.".into(),
            )
            .expect("record Hermes exchange");

        let pi = store
            .move_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Pi,
                    harness_host_id: Some("workstation".into()),
                    host_id: "air-m4".into(),
                    model_id: "qwen".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("move to Pi");
        assert_eq!(pi.handoff_from_session_id, Some(hermes.session_id));
        assert_eq!(pi.handoff_status, Some(ChannelHandoffStatus::Pending));
        assert_eq!(
            pi.native_archive_status,
            Some(ChannelNativeArchiveStatus::Pending)
        );
        assert_eq!(pi.project.as_deref(), Some("agent-relay"));

        let first_pi_prompt = store.delivery_text(&pi, "Start implementing it.");
        assert!(first_pi_prompt.contains("Let's build a portable handoff."));
        assert!(first_pi_prompt.contains("Use a durable transcript and explicit state."));
        assert!(first_pi_prompt.contains("Start implementing it."));

        store
            .record_exchange(
                &pi,
                Some("message-2".into()),
                "Start implementing it.".into(),
                "I'll begin in the selected project.".into(),
            )
            .expect("record first Pi exchange");
        let completed = store
            .complete_handoff(&address, pi.session_id)
            .expect("complete handoff");
        assert_eq!(
            completed.handoff_status,
            Some(ChannelHandoffStatus::Completed)
        );
        assert!(completed.handoff_completed_at_ms.is_some());
        let archive_failed = store
            .record_native_archive_result(
                &address,
                pi.session_id,
                hermes.session_id,
                Err("database busy".into()),
            )
            .expect("record native archive failure");
        assert_eq!(
            archive_failed.native_archive_status,
            Some(ChannelNativeArchiveStatus::Failed)
        );
        assert_eq!(
            archive_failed.native_archive_error.as_deref(),
            Some("database busy")
        );
        let archive_completed = store
            .record_native_archive_result(&address, pi.session_id, hermes.session_id, Ok(()))
            .expect("complete native archive");
        assert_eq!(
            archive_completed.native_archive_status,
            Some(ChannelNativeArchiveStatus::Completed)
        );
        assert!(store
            .get_session(&address, hermes.session_id)
            .expect("source route")
            .native_archived_at_ms
            .is_some());
        let restored = store
            .mark_native_unarchived(&address, hermes.session_id)
            .expect("mark native session restored");
        assert!(restored.native_archived_at_ms.is_none());
        assert_eq!(store.delivery_text(&completed, "Continue."), "Continue.");

        let opencode = store
            .move_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::OpenCode,
                    harness_host_id: Some("workstation".into()),
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("move from Pi to OpenCode");
        let chained_prompt = store.delivery_text(&opencode, "Continue in OpenCode.");
        assert!(chained_prompt.contains("Let's build a portable handoff."));
        assert!(chained_prompt.contains("I'll begin in the selected project."));
        assert!(chained_prompt.contains("Continue in OpenCode."));

        let reloaded = ChannelRouteStore::new(&directory).expect("reload route store");
        assert_eq!(reloaded.exchanges(&address, hermes.session_id).len(), 1);
        assert_eq!(reloaded.exchanges(&address, pi.session_id).len(), 1);
        assert_eq!(
            reloaded
                .cached_exchange(&address, pi.session_id, Some("message-2"))
                .expect("cached Pi exchange")
                .assistant_text,
            "I'll begin in the selected project."
        );
        fs::remove_dir_all(directory).expect("remove handoff test directory");
    }

    #[test]
    fn transcript_message_ids_are_idempotent_within_a_session() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-transcript-idempotency-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let address = ChannelAddress {
            channel: "photon".into(),
            account_id: "personal".into(),
            conversation_id: "duplicate".into(),
        };
        let store = ChannelRouteStore::new(&directory).expect("create route store");
        let route = store
            .start_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::Hermes,
                    harness_host_id: None,
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: None,
                },
            )
            .expect("start session");
        store
            .record_exchange(
                &route,
                Some("same-message".into()),
                "first".into(),
                "first reply".into(),
            )
            .unwrap();
        let replay = store
            .record_exchange(
                &route,
                Some("same-message".into()),
                "duplicate".into(),
                "duplicate reply".into(),
            )
            .unwrap();
        assert_eq!(replay.user_text, "first");
        assert_eq!(store.exchanges(&address, route.session_id).len(), 1);
        fs::remove_dir_all(directory).expect("remove transcript test directory");
    }

    #[test]
    fn handoff_prompt_omits_an_exchange_larger_than_its_budget() {
        let exchange = ChannelExchange {
            address: ChannelAddress {
                channel: "photon".into(),
                account_id: "personal".into(),
                conversation_id: "large".into(),
            },
            session_id: 1,
            external_message_id: Some("large-message".into()),
            user_text: "x".repeat(MAX_HANDOFF_BYTES + 1),
            assistant_text: "reply".into(),
            completed_at_ms: now_ms(),
        };
        let prompt = render_handoff_prompt(&[exchange], "Continue.");
        assert!(prompt.len() < MAX_HANDOFF_BYTES);
        assert!(prompt.contains("1 older exchange(s) were omitted"));
        assert!(prompt.ends_with("Continue."));
    }

    #[test]
    fn move_archives_the_old_session_and_resume_reactivates_it() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-channel-sessions-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let address = ChannelAddress {
            channel: "photon".into(),
            account_id: "personal".into(),
            conversation_id: "chat-2".into(),
        };
        let store = ChannelRouteStore::new(&directory).expect("create route store");
        let first = store
            .start_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: Some("Feature idea".into()),
                    harness: ChannelHarness::Hermes,
                    harness_host_id: Some("m1-pro".into()),
                    host_id: "m1-pro".into(),
                    model_id: "gemma".into(),
                    project: None,
                },
            )
            .expect("start brainstorm");
        let second = store
            .start_session(
                address.clone(),
                ChannelRouteTarget {
                    conversation_label: None,
                    harness: ChannelHarness::OpenCode,
                    harness_host_id: Some("workstation".into()),
                    host_id: "workstation".into(),
                    model_id: "qwen".into(),
                    project: Some("agent-relay".into()),
                },
            )
            .expect("move to coding");

        assert_ne!(first.session_id, second.session_id);
        assert_eq!(store.sessions(&address, false), vec![second.clone()]);
        assert_eq!(store.sessions(&address, true).len(), 2);
        let resumed = store
            .resume(&address, first.session_id)
            .expect("resume brainstorm");
        assert_eq!(resumed.session_id, first.session_id);
        assert_eq!(store.get(&address).unwrap().session_id, first.session_id);
        assert!(store
            .get_session(&address, second.session_id)
            .unwrap()
            .archived_at_ms
            .is_some());
        fs::remove_dir_all(directory).expect("remove session test directory");
    }

    #[test]
    fn migrates_a_legacy_sticky_route_to_a_numbered_session() {
        let directory = std::env::temp_dir().join(format!(
            "agentrelay-channel-migration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&directory).expect("create migration directory");
        fs::write(
            directory.join(ROUTES_FILE_NAME),
            r#"[{
                "channel":"photon",
                "account_id":"personal",
                "conversation_id":"legacy-chat",
                "harness":"hermes",
                "host_id":"m1-pro",
                "model_id":"gemma",
                "updated_at_ms":1
            }]"#,
        )
        .expect("write legacy route");

        let store = ChannelRouteStore::new(&directory).expect("migrate route store");
        let route = store.list().pop().expect("migrated session");
        assert_eq!(route.session_id, 1);
        assert!(route.archived_at_ms.is_none());
        let reloaded = ChannelRouteStore::new(&directory).expect("reload migrated route store");
        assert_eq!(reloaded.list()[0].session_id, 1);
        fs::remove_dir_all(directory).expect("remove migration test directory");
    }
}
