//! Host-owned inference reservations. Tokens are ephemeral, single-use and never telemetry.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Admissions(Mutex<HashMap<String, Entry>>);

struct Entry {
    model: String,
    expires: Option<Instant>,
}

pub(crate) struct SlotLease {
    pool: Arc<Admissions>,
    token: String,
    release: bool,
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if self.release {
            self.pool
                .0
                .lock()
                .expect("admissions poisoned")
                .remove(&self.token);
        }
    }
}

impl Admissions {
    /// The caller samples external runtime activity; our lock closes races between Relay callers.
    pub(crate) fn reserve(
        self: &Arc<Self>,
        model: &str,
        slots: u32,
        observed: u32,
    ) -> Result<SlotLease, String> {
        let mut entries = self.0.lock().expect("admissions poisoned");
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
        entries.insert(
            token.clone(),
            Entry {
                model: model.into(),
                expires: Some(now + Duration::from_secs(630)),
            },
        );
        Ok(SlotLease {
            pool: self.clone(),
            token,
            release: true,
        })
    }

    pub(crate) fn claim(self: &Arc<Self>, token: &str, model: &str) -> Result<SlotLease, String> {
        let mut entries = self.0.lock().expect("admissions poisoned");
        let entry = entries
            .get_mut(token)
            .ok_or("Reservation missing or expired; no prompt was submitted")?;
        if entry.model != model || !entry.expires.is_some_and(|t| t > Instant::now()) {
            return Err("Reservation expired, already used, or for another model".into());
        }
        entry.expires = None;
        Ok(SlotLease {
            pool: self.clone(),
            token: token.into(),
            release: true,
        })
    }

    /// Pinned calls retain their existing queueing behavior, but count against Auto capacity.
    pub(crate) fn track(self: &Arc<Self>, model: &str) -> SlotLease {
        let token = uuid::Uuid::new_v4().to_string();
        self.0.lock().expect("admissions poisoned").insert(
            token.clone(),
            Entry {
                model: model.into(),
                expires: None,
            },
        );
        SlotLease {
            pool: self.clone(),
            token,
            release: true,
        }
    }

    pub(crate) fn release_pending(&self, token: &str) {
        let mut entries = self.0.lock().expect("admissions poisoned");
        if entries.get(token).is_some_and(|e| e.expires.is_some()) {
            entries.remove(token);
        }
    }
}

impl SlotLease {
    pub(crate) fn into_ticket(mut self) -> String {
        self.pool
            .0
            .lock()
            .expect("admissions poisoned")
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
        p.0.lock().unwrap().get_mut(&ticket).unwrap().expires = Some(Instant::now());
        assert!(p.claim(&ticket, "q").is_err());
        let ticket = p.reserve("q", 1, 0).unwrap().into_ticket();
        p.release_pending(&ticket);
        assert!(p.claim(&ticket, "q").is_err());
        assert!(p.reserve("q", 1, 0).is_ok());
    }
}
