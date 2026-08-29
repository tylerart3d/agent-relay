use std::{
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{validate_channel_gateway_config, ChannelGatewayConfig},
    domain::{ConnectionState, FleetSnapshot, GatewayRuntimeState, GatewayRuntimeStatus},
};

const GATEWAY_HEARTBEAT_STALE_AFTER_MS: u64 = 30_000;

#[derive(Clone, Debug, Deserialize)]
pub struct GatewayHeartbeat {
    pub state: GatewayRuntimeState,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDecisionMode {
    Active,
    Standby,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GatewayDecision {
    pub mode: GatewayDecisionMode,
    pub host_id: String,
    pub reason: String,
    pub retry_after_ms: u64,
}

pub struct GatewayCoordinator {
    local_host_id: String,
    config: RwLock<ChannelGatewayConfig>,
    local_status: RwLock<Option<GatewayRuntimeStatus>>,
    primary_unavailable_since: Mutex<Option<Instant>>,
}

impl GatewayCoordinator {
    pub fn new(local_host_id: String, config: ChannelGatewayConfig) -> Self {
        Self {
            local_host_id,
            config: RwLock::new(config),
            local_status: RwLock::new(None),
            primary_unavailable_since: Mutex::new(None),
        }
    }

    pub fn config(&self) -> ChannelGatewayConfig {
        self.config.read().expect("gateway config poisoned").clone()
    }

    pub fn update_config(&self, config: ChannelGatewayConfig) -> Result<(), String> {
        validate_channel_gateway_config(&config)?;
        *self.config.write().expect("gateway config poisoned") = config;
        *self
            .primary_unavailable_since
            .lock()
            .expect("gateway failover timer poisoned") = None;
        Ok(())
    }

    pub fn heartbeat(&self, heartbeat: GatewayHeartbeat) -> GatewayRuntimeStatus {
        let status = GatewayRuntimeStatus {
            state: heartbeat.state,
            host_id: self.local_host_id.clone(),
            last_seen_ms: now_ms(),
            error: heartbeat.error,
        };
        *self.local_status.write().expect("gateway status poisoned") = Some(status.clone());
        status
    }

    pub fn local_status(&self) -> Option<GatewayRuntimeStatus> {
        self.local_status
            .read()
            .expect("gateway status poisoned")
            .clone()
    }

    pub fn decision(&self, snapshot: &FleetSnapshot) -> GatewayDecision {
        self.decision_at(snapshot, Instant::now(), now_ms())
    }

    fn decision_at(
        &self,
        snapshot: &FleetSnapshot,
        now: Instant,
        timestamp_ms: u64,
    ) -> GatewayDecision {
        let config = self.config();
        let active = |status: &Option<GatewayRuntimeStatus>| {
            status.as_ref().is_some_and(|status| {
                status.state == GatewayRuntimeState::Active
                    && timestamp_ms.saturating_sub(status.last_seen_ms)
                        <= GATEWAY_HEARTBEAT_STALE_AFTER_MS
            })
        };
        let decision = |mode, reason: &str| GatewayDecision {
            mode,
            host_id: self.local_host_id.clone(),
            reason: reason.into(),
            retry_after_ms: 5_000,
        };

        if config.primary_host_id.as_deref() == Some(self.local_host_id.as_str()) {
            let secondary_active = config.secondary_host_id.as_deref().is_some_and(|host_id| {
                snapshot
                    .hosts
                    .iter()
                    .find(|host| host.id == host_id)
                    .is_some_and(|host| active(&host.channel_gateway))
            });
            return if secondary_active {
                decision(
                    GatewayDecisionMode::Standby,
                    "the standby gateway already owns the Photon connection",
                )
            } else {
                decision(GatewayDecisionMode::Active, "this is the preferred gateway")
            };
        }

        if config.secondary_host_id.as_deref() != Some(self.local_host_id.as_str()) {
            return decision(
                GatewayDecisionMode::Disabled,
                "this machine is not an eligible gateway host",
            );
        }
        if !config.automatic_failover {
            return decision(
                GatewayDecisionMode::Standby,
                "automatic failover is disabled",
            );
        }

        let primary = config
            .primary_host_id
            .as_deref()
            .and_then(|host_id| snapshot.hosts.iter().find(|host| host.id == host_id));
        if primary.is_some_and(|host| active(&host.channel_gateway)) {
            *self
                .primary_unavailable_since
                .lock()
                .expect("gateway failover timer poisoned") = None;
            return decision(
                GatewayDecisionMode::Standby,
                "the preferred gateway is active",
            );
        }

        if active(&self.local_status()) {
            return decision(
                GatewayDecisionMode::Active,
                "this standby gateway has already taken ownership",
            );
        }

        let primary_available = primary.is_some_and(|host| {
            host.connection != ConnectionState::Offline
                && host.channel_gateway.as_ref().is_some_and(|status| {
                    matches!(
                        status.state,
                        GatewayRuntimeState::Active
                            | GatewayRuntimeState::Standby
                            | GatewayRuntimeState::Starting
                    ) && timestamp_ms.saturating_sub(status.last_seen_ms)
                        <= GATEWAY_HEARTBEAT_STALE_AFTER_MS
                })
        });
        if primary_available {
            *self
                .primary_unavailable_since
                .lock()
                .expect("gateway failover timer poisoned") = None;
            return decision(
                GatewayDecisionMode::Standby,
                "the preferred gateway is available",
            );
        }

        let mut unavailable_since = self
            .primary_unavailable_since
            .lock()
            .expect("gateway failover timer poisoned");
        let started = unavailable_since.get_or_insert(now);
        let delay = Duration::from_secs(config.failover_after_seconds);
        if now.duration_since(*started) >= delay {
            decision(
                GatewayDecisionMode::Active,
                "the preferred gateway exceeded the failover window",
            )
        } else {
            let remaining = delay.saturating_sub(now.duration_since(*started));
            GatewayDecision {
                retry_after_ms: remaining.as_millis().min(5_000) as u64,
                ..decision(
                    GatewayDecisionMode::Standby,
                    "waiting for the automatic failover window",
                )
            }
        }
    }
}

pub type SharedGatewayCoordinator = Arc<GatewayCoordinator>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{HostStatus, LlamaSwapStatus, PeerApiStatus};

    fn snapshot(local: &str) -> FleetSnapshot {
        FleetSnapshot {
            local_host_id: local.into(),
            config_path: String::new(),
            proxy_endpoint: String::new(),
            refreshed_at_ms: 0,
            peer_api: PeerApiStatus::default(),
            hosts: Vec::new(),
            opencode: Default::default(),
            hermes: Default::default(),
            hermes_cli: Default::default(),
            codex: Default::default(),
            claude_code: Default::default(),
            pi: Default::default(),
            copilot: Default::default(),
            vscode: Default::default(),
        }
    }

    fn host(
        id: &str,
        connection: ConnectionState,
        state: Option<GatewayRuntimeState>,
    ) -> HostStatus {
        HostStatus {
            id: id.into(),
            display_name: id.into(),
            address: id.into(),
            hardware: String::new(),
            connection,
            models: Vec::new(),
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
            channel_gateway: state.map(|state| GatewayRuntimeStatus {
                state,
                host_id: id.into(),
                last_seen_ms: 1_000,
                error: None,
            }),
        }
    }

    #[test]
    fn primary_is_active_by_default() {
        let coordinator = GatewayCoordinator::new(
            "workstation".into(),
            ChannelGatewayConfig {
                primary_host_id: Some("workstation".into()),
                secondary_host_id: Some("m1-pro".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            coordinator.decision(&snapshot("workstation")).mode,
            GatewayDecisionMode::Active
        );
    }

    #[test]
    fn ineligible_host_is_disabled() {
        let coordinator = GatewayCoordinator::new(
            "air-m4".into(),
            ChannelGatewayConfig {
                primary_host_id: Some("workstation".into()),
                secondary_host_id: Some("m1-pro".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            coordinator.decision(&snapshot("air-m4")).mode,
            GatewayDecisionMode::Disabled
        );
    }

    #[test]
    fn recovered_primary_does_not_preempt_an_active_standby() {
        let coordinator = GatewayCoordinator::new(
            "workstation".into(),
            ChannelGatewayConfig {
                primary_host_id: Some("workstation".into()),
                secondary_host_id: Some("m1-pro".into()),
                ..Default::default()
            },
        );
        let mut fleet = snapshot("workstation");
        fleet.hosts.push(host(
            "m1-pro",
            ConnectionState::Online,
            Some(GatewayRuntimeState::Active),
        ));
        assert_eq!(
            coordinator.decision_at(&fleet, Instant::now(), 1_000).mode,
            GatewayDecisionMode::Standby
        );
    }

    #[test]
    fn standby_takes_over_only_after_the_failure_window() {
        let coordinator = GatewayCoordinator::new(
            "m1-pro".into(),
            ChannelGatewayConfig {
                primary_host_id: Some("workstation".into()),
                secondary_host_id: Some("m1-pro".into()),
                failover_after_seconds: 60,
                ..Default::default()
            },
        );
        let mut fleet = snapshot("m1-pro");
        fleet
            .hosts
            .push(host("workstation", ConnectionState::Offline, None));
        let started = Instant::now();
        assert_eq!(
            coordinator.decision_at(&fleet, started, 1_000).mode,
            GatewayDecisionMode::Standby
        );
        assert_eq!(
            coordinator
                .decision_at(&fleet, started + Duration::from_secs(61), 62_000)
                .mode,
            GatewayDecisionMode::Active
        );
    }

    #[test]
    fn active_standby_keeps_ownership_when_primary_returns_idle() {
        let coordinator = GatewayCoordinator::new(
            "m1-pro".into(),
            ChannelGatewayConfig {
                primary_host_id: Some("workstation".into()),
                secondary_host_id: Some("m1-pro".into()),
                ..Default::default()
            },
        );
        coordinator.heartbeat(GatewayHeartbeat {
            state: GatewayRuntimeState::Active,
            error: None,
        });
        let mut fleet = snapshot("m1-pro");
        fleet.hosts.push(host(
            "workstation",
            ConnectionState::Online,
            Some(GatewayRuntimeState::Standby),
        ));
        assert_eq!(
            coordinator.decision(&fleet).mode,
            GatewayDecisionMode::Active
        );
    }
}
