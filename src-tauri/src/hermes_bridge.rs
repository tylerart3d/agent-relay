use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const CLIENT_TTL: Duration = Duration::from_secs(10);
const INTENT_TTL: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Deserialize)]
pub struct HermesPresence {
    pub client_id: String,
    pub session_id: Option<String>,
    #[serde(default)]
    pub visible_model: String,
    pub focused_at_ms: u64,
    #[serde(default)]
    pub last_handled_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HermesSwitchCommand {
    pub revision: u64,
    pub model: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HermesPresenceResponse {
    pub command: Option<HermesSwitchCommand>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HermesSwitchAck {
    pub client_id: String,
    pub revision: u64,
    pub session_id: Option<String>,
    pub state: String,
    #[serde(default)]
    pub deferred: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HermesDeliveryResult {
    Switched(HermesSwitchAck),
    Deferred(HermesSwitchAck),
    Error(HermesSwitchAck),
    TimedOut,
    Superseded,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HermesBridgeStatus {
    pub client_count: usize,
    pub active_chat_count: usize,
    pub visible_model: Option<String>,
    pub pending_revision: Option<u64>,
    pub pending_model: Option<String>,
    pub last_ack: Option<HermesSwitchAck>,
}

#[derive(Clone, Debug)]
struct Client {
    session_id: Option<String>,
    visible_model: String,
    focused_at_ms: u64,
    seen_at_ms: u64,
}

#[derive(Clone, Debug)]
struct Intent {
    revision: u64,
    model: String,
    created_at_ms: u64,
    dispatched_to: Option<String>,
    dispatched_session_id: Option<String>,
}

#[derive(Default)]
struct BridgeState {
    clients: HashMap<String, Client>,
    intent: Option<Intent>,
    last_ack: Option<HermesSwitchAck>,
    next_revision: u64,
}

pub struct HermesBridge {
    state: Mutex<BridgeState>,
    changed: Condvar,
}

impl Default for HermesBridge {
    fn default() -> Self {
        Self {
            state: Mutex::new(BridgeState::default()),
            changed: Condvar::new(),
        }
    }
}

impl HermesBridge {
    pub fn publish(&self, model: String) -> u64 {
        let mut state = self.state.lock().expect("Hermes bridge state poisoned");
        state.next_revision = state.next_revision.max(now_ms()).saturating_add(1);
        let revision = state.next_revision;
        state.intent = Some(Intent {
            revision,
            model,
            created_at_ms: now_ms(),
            dispatched_to: None,
            dispatched_session_id: None,
        });
        self.changed.notify_all();
        revision
    }

    pub fn presence(&self, presence: HermesPresence) -> HermesPresenceResponse {
        let now = now_ms();
        let mut state = self.state.lock().expect("Hermes bridge state poisoned");
        state.clients.retain(|_, client| {
            now.saturating_sub(client.seen_at_ms) <= CLIENT_TTL.as_millis() as u64
        });
        state.clients.insert(
            presence.client_id.clone(),
            Client {
                session_id: presence.session_id,
                visible_model: presence.visible_model,
                focused_at_ms: presence.focused_at_ms,
                seen_at_ms: now,
            },
        );

        let expired = state.intent.as_ref().is_some_and(|intent| {
            now.saturating_sub(intent.created_at_ms) > INTENT_TTL.as_millis() as u64
        });
        if expired {
            state.intent = None;
            self.changed.notify_all();
            return HermesPresenceResponse::default();
        }

        let Some(intent) = state.intent.as_ref() else {
            return HermesPresenceResponse::default();
        };
        if presence.last_handled_revision >= intent.revision {
            return HermesPresenceResponse::default();
        }

        let target = if let Some(client_id) = intent.dispatched_to.as_ref() {
            state
                .clients
                .contains_key(client_id)
                .then(|| (client_id.clone(), intent.dispatched_session_id.clone()))
        } else {
            state
                .clients
                .iter()
                .max_by_key(|(client_id, client)| {
                    (client.focused_at_ms, client.seen_at_ms, *client_id)
                })
                .map(|(client_id, client)| (client_id.clone(), client.session_id.clone()))
        };
        let Some((target_id, target_session_id)) = target else {
            return HermesPresenceResponse::default();
        };
        if target_id != presence.client_id {
            return HermesPresenceResponse::default();
        }

        let intent = state
            .intent
            .as_mut()
            .expect("Hermes intent was checked above");
        if intent.dispatched_to.is_none() {
            intent.dispatched_to = Some(target_id);
            intent.dispatched_session_id = target_session_id.clone();
        }
        HermesPresenceResponse {
            command: Some(HermesSwitchCommand {
                revision: intent.revision,
                model: intent.model.clone(),
                session_id: target_session_id,
            }),
        }
    }

    /// Records an acknowledgement only when it matches the exact client and
    /// session to which the current revision was dispatched.
    pub fn acknowledge(&self, ack: HermesSwitchAck) -> bool {
        let valid_state = matches!(ack.state.as_str(), "switched" | "deferred" | "error");
        let result_is_consistent = match ack.state.as_str() {
            "switched" => !ack.deferred && ack.error.is_none(),
            "deferred" => ack.deferred && ack.error.is_none(),
            "error" => ack.error.is_some(),
            _ => false,
        };
        if ack.client_id.trim().is_empty()
            || ack
                .session_id
                .as_ref()
                .is_some_and(|session_id| session_id.trim().is_empty())
            || !valid_state
            || !result_is_consistent
        {
            return false;
        }
        let mut state = self.state.lock().expect("Hermes bridge state poisoned");
        let matches_delivery = state.intent.as_ref().is_some_and(|intent| {
            intent.revision == ack.revision
                && intent.dispatched_to.as_deref() == Some(ack.client_id.as_str())
                && intent.dispatched_session_id == ack.session_id
        });
        if matches_delivery {
            state.last_ack = Some(ack);
            state.intent = None;
            self.changed.notify_all();
            true
        } else {
            false
        }
    }

    pub fn wait_for_delivery(&self, revision: u64, timeout: Duration) -> HermesDeliveryResult {
        let started = Instant::now();
        let mut state = self.state.lock().expect("Hermes bridge state poisoned");
        loop {
            if let Some(ack) = state
                .last_ack
                .as_ref()
                .filter(|ack| ack.revision == revision)
                .cloned()
            {
                return match ack.state.as_str() {
                    "switched" => HermesDeliveryResult::Switched(ack),
                    "deferred" => HermesDeliveryResult::Deferred(ack),
                    "error" => HermesDeliveryResult::Error(ack),
                    _ => unreachable!("ack state is validated before storage"),
                };
            }

            match state.intent.as_ref() {
                Some(intent) if intent.revision == revision => {}
                Some(intent) if intent.revision > revision => {
                    return HermesDeliveryResult::Superseded;
                }
                _ => return HermesDeliveryResult::TimedOut,
            }

            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return HermesDeliveryResult::TimedOut;
            };
            if remaining.is_zero() {
                return HermesDeliveryResult::TimedOut;
            }
            let (next_state, wait_result) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("Hermes bridge state poisoned while waiting");
            state = next_state;
            if wait_result.timed_out() {
                return HermesDeliveryResult::TimedOut;
            }
        }
    }

    pub fn status(&self) -> HermesBridgeStatus {
        let now = now_ms();
        let mut state = self.state.lock().expect("Hermes bridge state poisoned");
        state.clients.retain(|_, client| {
            now.saturating_sub(client.seen_at_ms) <= CLIENT_TTL.as_millis() as u64
        });
        if state.intent.as_ref().is_some_and(|intent| {
            now.saturating_sub(intent.created_at_ms) > INTENT_TTL.as_millis() as u64
        }) {
            state.intent = None;
            self.changed.notify_all();
        }
        let intent = state.intent.as_ref();
        HermesBridgeStatus {
            client_count: state.clients.len(),
            active_chat_count: state
                .clients
                .values()
                .filter(|client| client.session_id.is_some())
                .count(),
            visible_model: state
                .clients
                .values()
                .max_by_key(|client| (client.focused_at_ms, client.seen_at_ms))
                .map(|client| client.visible_model.clone())
                .filter(|model| !model.is_empty()),
            pending_revision: intent.map(|intent| intent.revision),
            pending_model: intent.map(|intent| intent.model.clone()),
            last_ack: state.last_ack.clone(),
        }
    }
}

pub type SharedHermesBridge = Arc<HermesBridge>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(client: &str, session: Option<&str>, focused_at_ms: u64) -> HermesPresence {
        HermesPresence {
            client_id: client.into(),
            session_id: session.map(str::to_owned),
            visible_model: "old-model".into(),
            focused_at_ms,
            last_handled_revision: 0,
        }
    }

    #[test]
    fn sends_a_switch_to_the_most_recently_focused_chat() {
        let bridge = HermesBridge::default();
        assert_eq!(
            bridge
                .presence(presence("old", Some("session-a"), 10))
                .command,
            None
        );
        assert_eq!(
            bridge
                .presence(presence("new", Some("session-b"), 20))
                .command,
            None
        );
        let revision = bridge.publish("workstation/bonsai".into());

        assert_eq!(
            bridge
                .presence(presence("old", Some("session-a"), 10))
                .command,
            None
        );
        assert_eq!(
            bridge
                .presence(presence("new", Some("session-b"), 20))
                .command,
            Some(HermesSwitchCommand {
                revision,
                model: "workstation/bonsai".into(),
                session_id: Some("session-b".into()),
            })
        );
    }

    #[test]
    fn revisions_remain_new_to_a_client_after_the_bridge_restarts() {
        let bridge = HermesBridge::default();
        let mut existing_client = presence("desktop", Some("session-a"), 10);
        existing_client.last_handled_revision = 42;
        assert_eq!(bridge.presence(existing_client.clone()).command, None);

        let revision = bridge.publish("m1-pro/qwen".into());

        assert!(revision > existing_client.last_handled_revision);
        assert_eq!(
            bridge.presence(existing_client).command,
            Some(HermesSwitchCommand {
                revision,
                model: "m1-pro/qwen".into(),
                session_id: Some("session-a".into()),
            })
        );
    }

    #[test]
    fn sends_a_new_session_switch_when_hermes_has_no_active_chat() {
        let bridge = HermesBridge::default();
        let revision = bridge.publish("workstation/bonsai".into());
        assert_eq!(
            bridge.presence(presence("window", None, 10)).command,
            Some(HermesSwitchCommand {
                revision,
                model: "workstation/bonsai".into(),
                session_id: None,
            })
        );
    }

    #[test]
    fn does_not_repeat_a_handled_revision() {
        let bridge = HermesBridge::default();
        let revision = bridge.publish("workstation/bonsai".into());
        let mut current = presence("window", Some("session-a"), 10);
        current.last_handled_revision = revision;
        assert_eq!(bridge.presence(current).command, None);
    }

    #[test]
    fn successful_ack_consumes_the_intent_and_wakes_waiters() {
        let bridge = Arc::new(HermesBridge::default());
        bridge.presence(presence("desktop", None, 10));
        let revision = bridge.publish("workstation/bonsai".into());
        assert!(bridge
            .presence(presence("desktop", None, 10))
            .command
            .is_some());

        let waiting = bridge.clone();
        let waiter =
            std::thread::spawn(move || waiting.wait_for_delivery(revision, Duration::from_secs(1)));
        bridge.acknowledge(HermesSwitchAck {
            client_id: "desktop".into(),
            revision,
            session_id: None,
            state: "switched".into(),
            deferred: false,
            error: None,
        });

        assert!(matches!(
            waiter.join().unwrap(),
            HermesDeliveryResult::Switched(_)
        ));
        assert_eq!(bridge.status().pending_revision, None);
        assert_eq!(bridge.presence(presence("desktop", None, 10)).command, None);
    }

    #[test]
    fn deferred_and_error_results_are_authoritative() {
        for (state, deferred, error) in [
            ("deferred", true, None),
            ("error", false, Some("draft did not open".to_owned())),
        ] {
            let bridge = HermesBridge::default();
            bridge.presence(presence("desktop", Some("old"), 10));
            let revision = bridge.publish("m1-pro/qwen".into());
            bridge.presence(presence("desktop", Some("old"), 10));
            bridge.acknowledge(HermesSwitchAck {
                client_id: "desktop".into(),
                revision,
                session_id: Some("old".into()),
                state: state.into(),
                deferred,
                error,
            });

            let result = bridge.wait_for_delivery(revision, Duration::ZERO);
            assert!(matches!(
                (state, result),
                ("deferred", HermesDeliveryResult::Deferred(_))
                    | ("error", HermesDeliveryResult::Error(_))
            ));
        }
    }

    #[test]
    fn delivery_is_pinned_to_the_first_target_and_rejects_impostor_acks() {
        let bridge = HermesBridge::default();
        bridge.presence(presence("first", Some("session-a"), 20));
        let revision = bridge.publish("workstation/bonsai".into());
        assert!(bridge
            .presence(presence("first", Some("session-a"), 20))
            .command
            .is_some());

        assert_eq!(
            bridge
                .presence(presence("later", Some("session-b"), 100))
                .command,
            None
        );
        bridge.acknowledge(HermesSwitchAck {
            client_id: "later".into(),
            revision,
            session_id: Some("session-b".into()),
            state: "switched".into(),
            deferred: false,
            error: None,
        });
        assert_eq!(bridge.status().pending_revision, Some(revision));
    }
}
