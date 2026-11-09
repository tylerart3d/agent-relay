//! Host-owned inference reservations. Tokens are ephemeral, single-use and never telemetry.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Admissions(Mutex<AdmissionState>);

#[derive(Default)]
struct AdmissionState {
    entries: HashMap<String, Entry>,
    reclaims: HashMap<String, String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ReclaimStatus {
    pub models: Vec<String>,
    pub target: Option<String>,
    pub active_requests: u32,
    pub reserved: bool,
}

pub(crate) struct ReclaimLease {
    pool: Arc<Admissions>,
    token: String,
}
impl Drop for ReclaimLease {
    fn drop(&mut self) {
        self.pool
            .0
            .lock()
            .expect("admissions poisoned")
            .reclaims
            .remove(&self.token);
    }
}

const BACKGROUND_BUSY: &str =
    "host_reclaimed: reserved for interactive use; retry background work later";

impl AdmissionState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, e| e.expires.is_none_or(|t| t > now));
    }
    fn blocks_background(&self) -> bool {
        !self.reclaims.is_empty() || self.entries.values().any(|e| e.interactive)
    }
}

struct Entry {
    model: String,
    interactive: bool,
    cancel: tokio::sync::watch::Sender<bool>,
    expires: Option<Instant>,
}

pub(crate) struct SlotLease {
    pool: Arc<Admissions>,
    token: String,
    release: bool,
    cancelled: tokio::sync::watch::Receiver<bool>,
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if self.release {
            let mut state = self.pool.0.lock().expect("admissions poisoned");
            state.entries.remove(&self.token);
        }
    }
}

impl Admissions {
    /// The caller samples external runtime activity; our lock closes races between Relay callers.
    #[cfg(test)]
    pub(crate) fn reserve(
        self: &Arc<Self>,
        model: &str,
        slots: u32,
        observed: u32,
    ) -> Result<SlotLease, String> {
        self.reserve_classified(model, slots, observed, false, false)
    }

    pub(crate) fn reserve_classified(
        self: &Arc<Self>,
        model: &str,
        slots: u32,
        observed: u32,
        background: bool,
        enabled: bool,
    ) -> Result<SlotLease, String> {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.prune();
        if background && state.blocks_background() {
            return Err(BACKGROUND_BUSY.into());
        }
        if state.reclaims.values().any(|target| target != model) {
            return Err("capacity_busy: host is being reclaimed".into());
        }
        let entries = &mut state.entries;
        let now = Instant::now();
        entries.retain(|_, e| e.expires.is_none_or(|t| t > now));
        if entries.values().any(|e| e.model != model) {
            return Err("capacity_busy: another model has reserved work".into());
        }
        let active = entries.values().filter(|e| e.expires.is_none()).count() as u32;
        let pending = entries.len() as u32 - active;
        if active.max(observed).saturating_add(pending) >= slots.max(1) {
            return Err("capacity_busy: all model slots are occupied or reserved".into());
        }
        let token = uuid::Uuid::new_v4().to_string();
        // Covers preparation; dropping a cancelled preparation releases immediately.
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        entries.insert(
            token.clone(),
            Entry {
                model: model.into(),
                interactive: enabled && !background,
                cancel,
                expires: Some(now + Duration::from_secs(630)),
            },
        );
        Ok(SlotLease {
            pool: self.clone(),
            token,
            release: true,
            cancelled,
        })
    }

    pub(crate) fn claim(self: &Arc<Self>, token: &str, model: &str) -> Result<SlotLease, String> {
        let mut state = self.0.lock().expect("admissions poisoned");
        let entries = &mut state.entries;
        let entry = entries
            .get_mut(token)
            .ok_or("Reservation missing or expired; no prompt was submitted")?;
        if entry.model != model || !entry.expires.is_some_and(|t| t > Instant::now()) {
            return Err("Reservation expired, already used, or for another model".into());
        }
        entry.expires = None;
        let cancelled = entry.cancel.subscribe();
        Ok(SlotLease {
            pool: self.clone(),
            token: token.into(),
            release: true,
            cancelled,
        })
    }

    /// Classification and admission share the same lock as reclaim intent.
    pub(crate) fn track_checked(
        self: &Arc<Self>,
        model: &str,
        background: bool,
        policy_enabled: bool,
    ) -> Result<SlotLease, String> {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.prune();
        if background && state.blocks_background() {
            return Err(BACKGROUND_BUSY.into());
        }
        if state.reclaims.values().any(|target| target != model) && !background {
            return Err("capacity_busy: host is preparing a different interactive model".into());
        }
        let token = uuid::Uuid::new_v4().to_string();
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        state.entries.insert(
            token.clone(),
            Entry {
                model: model.into(),
                interactive: policy_enabled && !background,
                cancel,
                expires: None,
            },
        );
        Ok(SlotLease {
            pool: self.clone(),
            token,
            release: true,
            cancelled,
        })
    }

    #[cfg(test)]
    pub(crate) fn track(self: &Arc<Self>, model: &str) -> SlotLease {
        self.track_checked(model, false, false).unwrap()
    }

    pub(crate) fn begin_reclaim(
        self: &Arc<Self>,
        model: &str,
        background: &[String],
    ) -> Result<ReclaimLease, String> {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.prune();
        if state.reclaims.len() >= 64
            || background.iter().any(|m| m == model)
            || state.reclaims.values().any(|m| m != model)
            || state
                .entries
                .values()
                .any(|e| e.model != model && !background.contains(&e.model))
        {
            return Err("capacity_busy: cannot reclaim interactive work".into());
        }
        let token = uuid::Uuid::new_v4().to_string();
        state.reclaims.insert(token.clone(), model.into());
        Ok(ReclaimLease {
            pool: self.clone(),
            token,
        })
    }

    pub(crate) fn check_background(&self, background: bool) -> Result<(), String> {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.prune();
        if background && state.blocks_background() {
            Err(BACKGROUND_BUSY.into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn background_count(&self, models: &[String]) -> usize {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.prune();
        state
            .entries
            .values()
            .filter(|e| models.contains(&e.model))
            .count()
    }

    /// Revoke only the named background profile. Ready tickets cannot later claim it.
    pub(crate) fn cancel_model(&self, model: &str) {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.entries.retain(|_, e| {
            if e.model == model {
                e.cancel.send_replace(true);
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn snapshot(&self, models: Vec<String>) -> ReclaimStatus {
        let mut state = self.0.lock().expect("admissions poisoned");
        state.prune();
        ReclaimStatus {
            active_requests: state
                .entries
                .values()
                .filter(|e| models.contains(&e.model))
                .count() as u32,
            target: state.reclaims.values().next().cloned(),
            reserved: state.blocks_background(),
            models,
        }
    }

    pub(crate) fn release_pending(&self, token: &str) {
        let mut state = self.0.lock().expect("admissions poisoned");
        let entries = &mut state.entries;
        if entries.get(token).is_some_and(|e| e.expires.is_some()) {
            entries.remove(token);
        }
    }
}

impl SlotLease {
    pub(crate) fn cancellation(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancelled.clone()
    }

    pub(crate) fn into_ticket(mut self) -> String {
        self.pool
            .0
            .lock()
            .expect("admissions poisoned")
            .entries
            .get_mut(&self.token)
            .expect("reservation exists")
            .expires = Some(Instant::now() + Duration::from_secs(30));
        self.release = false;
        self.token.clone()
    }
}

#[derive(Default)]
pub(crate) struct Waiting(Mutex<HashMap<String, u32>>);
pub(crate) struct WaitLease {
    waiting: Arc<Waiting>,
    client: String,
}
impl Waiting {
    pub(crate) fn enter(self: &Arc<Self>, client: &str) -> Result<WaitLease, String> {
        let mut counts = self.0.lock().expect("queue poisoned");
        if counts.values().sum::<u32>() >= 64 {
            return Err("Auto waiting queue is full".into());
        }
        *counts.entry(client.into()).or_default() += 1;
        Ok(WaitLease {
            waiting: self.clone(),
            client: client.into(),
        })
    }
    pub(crate) fn snapshot(&self) -> HashMap<String, u32> {
        self.0.lock().expect("queue poisoned").clone()
    }
}
impl Drop for WaitLease {
    fn drop(&mut self) {
        let mut counts = self.waiting.0.lock().expect("queue poisoned");
        if let Some(n) = counts.get_mut(&self.client) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waiting_is_bounded_and_cancellation_removes_ui_state() {
        let q = Arc::new(Waiting::default());
        let guards: Vec<_> = (0..64).map(|_| q.enter("openbot").unwrap()).collect();
        assert_eq!(q.snapshot()["openbot"], 64);
        assert!(q.enter("hermes").is_err());
        drop(guards);
        assert!(q.snapshot().is_empty());
    }
    #[test]
    fn reservations_are_atomic_single_use_and_release_on_drop() {
        let p = Arc::new(Admissions::default());
        let ticket = p.reserve("q", 1, 0).unwrap().into_ticket();
        assert!(p.reserve("q", 1, 0).is_err());
        assert!(p.claim(&ticket, "other").is_err());
        let active = p.claim(&ticket, "q").unwrap();
        assert!(p.claim(&ticket, "q").is_err());
        p.release_pending(&ticket);
        assert!(p.reserve("q", 1, 0).is_err());
        drop(active);
        assert!(p.reserve("q", 1, 0).is_ok());
    }
    #[test]
    fn capacity_accounts_for_pinned_external_and_parallel_work() {
        let p = Arc::new(Admissions::default());
        let a = p.track("q");
        assert!(p.reserve("q", 1, 0).is_err());
        let b = p.reserve("q", 2, 1).unwrap();
        assert!(p.reserve("q", 2, 1).is_err());
        assert!(p.reserve("other", 4, 0).is_err());
        drop((a, b));
        assert!(p.reserve("q", 2, 2).is_err());
    }
    #[test]
    fn cancelled_and_expired_tickets_do_not_strand_capacity() {
        let p = Arc::new(Admissions::default());
        let ticket = p.reserve("q", 1, 0).unwrap().into_ticket();
        p.0.lock()
            .unwrap()
            .entries
            .get_mut(&ticket)
            .unwrap()
            .expires = Some(Instant::now());
        assert!(p.claim(&ticket, "q").is_err());
        let ticket = p.reserve("q", 1, 0).unwrap().into_ticket();
        p.release_pending(&ticket);
        assert!(p.claim(&ticket, "q").is_err());
        assert!(p.reserve("q", 1, 0).is_ok());
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    #[test]
    fn drain_blocks_new_work_but_keeps_existing_six_pages_alive() {
        let pool = Arc::new(Admissions::default());
        let models = vec!["ocr".into()];
        let pages: Vec<_> = (0..6)
            .map(|_| pool.track_checked("ocr", true, true).unwrap())
            .collect();
        let intent = pool.begin_reclaim("chat", &models).unwrap();
        assert_eq!(pool.background_count(&models), 6);
        assert!(pool.track_checked("ocr", true, true).is_err());
        assert!(pool.reserve_classified("ocr", 6, 0, true, true).is_err());
        assert!(pool.reserve_classified("chat", 1, 0, false, true).is_err());
        drop(pages);
        let ticket = pool
            .reserve_classified("chat", 1, 0, false, true)
            .unwrap()
            .into_ticket();
        drop(intent);
        assert!(pool.track_checked("ocr", true, true).is_err());
        let chat = pool.claim(&ticket, "chat").unwrap();
        drop(chat);
        // Residency protection belongs to the lifecycle gate, not a timer here.
        assert!(pool.track_checked("ocr", true, true).is_ok());
    }
    #[test]
    fn cancellation_removes_intent_without_starting_a_cooldown() {
        let pool = Arc::new(Admissions::default());
        let models = vec!["ocr".into()];
        let a = pool.begin_reclaim("chat", &models).unwrap();
        let b = pool.begin_reclaim("chat", &models).unwrap();
        assert!(pool.begin_reclaim("other", &models).is_err());
        drop(a);
        assert!(pool.check_background(true).is_err());
        drop(b);
        assert!(!pool.snapshot(models).reserved);
        assert!(pool.track_checked("ocr", true, true).is_ok());
    }
    #[test]
    fn never_reclaims_interactive_work_or_protects_an_expired_unclaimed_ticket() {
        let pool = Arc::new(Admissions::default());
        let chat = pool.track_checked("chat", false, true).unwrap();
        assert!(pool.begin_reclaim("other", &["ocr".into()]).is_err());
        drop(chat);
        let ticket = pool
            .reserve_classified("chat", 1, 0, false, true)
            .unwrap()
            .into_ticket();
        pool.0
            .lock()
            .unwrap()
            .entries
            .get_mut(&ticket)
            .unwrap()
            .expires = Some(Instant::now());
        assert!(pool.track_checked("ocr", true, true).is_ok());
    }
    #[test]
    fn forced_drain_signals_only_background_and_revokes_pending_tickets() {
        let pool = Arc::new(Admissions::default());
        let page = pool.track_checked("ocr", true, true).unwrap();
        let chat = pool.track_checked("chat", false, true).unwrap();
        let cancelled = page.cancellation();
        pool.cancel_model("ocr");
        assert!(*cancelled.borrow());
        assert!(!*chat.cancellation().borrow());
        assert_eq!(pool.background_count(&["ocr".into()]), 0);
        drop((page, chat));
        let ticket = pool
            .reserve_classified("ocr", 1, 0, true, true)
            .unwrap()
            .into_ticket();
        pool.cancel_model("ocr");
        assert!(pool.claim(&ticket, "ocr").is_err());
    }
}
