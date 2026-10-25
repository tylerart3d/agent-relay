//! Slot save/restore orchestration for capability-tested runtimes.
//!
//! The caller must hold an exclusive model lifecycle lease for the entire
//! operation. Ordinary inference must still pass through llama-swap; only these
//! idle, administrative operations address the loopback runtime directly.
use std::time::Duration;

use serde::Deserialize;

use crate::kv_cache::{CacheIdentity, CacheStore, Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotState {
    pub busy: bool,
    // llama.cpp omits this until the slot has a generation task, including
    // immediately after restoring into a freshly started process.
    pub tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct SavedState {
    pub tokens: u64,
    pub bytes: u64,
}

/// No prompt/response data crosses this interface. Implementations must have
/// passed the real process-restart benchmark, not just returned HTTP 200.
pub trait SlotBackend {
    fn state(&self) -> impl std::future::Future<Output = Result<SlotState, String>> + Send;
    fn save(
        &self,
        filename: &str,
    ) -> impl std::future::Future<Output = Result<SavedState, String>> + Send;
    fn restore(
        &self,
        filename: &str,
    ) -> impl std::future::Future<Output = Result<u64, String>> + Send;
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Empty,
    Busy,
    Saved(Snapshot),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Miss,
    Occupied,
    Restored(Snapshot),
}

/// Save failures retain the pending reservation. The native server can keep
/// writing after a client timeout; do not remove it until shutdown is confirmed.
pub async fn save_idle(
    store: &mut CacheStore,
    backend: &impl SlotBackend,
    identity: &CacheIdentity,
    now_ms: u64,
) -> Result<SaveOutcome, String> {
    let state = backend.state().await?;
    if state.busy {
        return Ok(SaveOutcome::Busy);
    }
    let Some(tokens) = state.tokens.filter(|tokens| *tokens != 0) else {
        return Ok(SaveOutcome::Empty);
    };
    let pending = store.reserve(identity, now_ms)?;
    let saved = backend.save(&pending.filename()).await?;
    if saved.tokens != tokens {
        return Err(
            "KV slot changed during save; preserve the pending file until runtime shutdown".into(),
        );
    }
    store
        .commit(&pending.id, saved.tokens, saved.bytes)
        .map(SaveOutcome::Saved)
}

/// Restore only into an empty, idle slot. A restore error requires a clean
/// runtime restart before cold inference: a timed-out transfer may be partial.
pub async fn restore_empty(
    store: &mut CacheStore,
    backend: &impl SlotBackend,
    identity: &CacheIdentity,
    now_ms: u64,
) -> Result<RestoreOutcome, String> {
    restore_snapshot(store, backend, identity, now_ms, false).await
}

/// Only the process owner may call this, immediately after starting a new
/// runtime and before admitting inference. Some llama.cpp builds omit token
/// counts before the first generation, even after a successful slot restore.
/// The explicit fresh-process guarantee allows that omission; response file,
/// slot, restored count and post-restore idle state must still be verified.
pub async fn restore_fresh(
    store: &mut CacheStore,
    backend: &impl SlotBackend,
    identity: &CacheIdentity,
    now_ms: u64,
) -> Result<RestoreOutcome, String> {
    restore_snapshot(store, backend, identity, now_ms, true).await
}

async fn restore_snapshot(
    store: &mut CacheStore,
    backend: &impl SlotBackend,
    identity: &CacheIdentity,
    now_ms: u64,
    fresh_process: bool,
) -> Result<RestoreOutcome, String> {
    let Some(mut snapshot) = store.candidate(identity, now_ms)? else {
        return Ok(RestoreOutcome::Miss);
    };
    let state = backend.state().await?;
    if state.busy || state.tokens.map_or(!fresh_process, |tokens| tokens != 0) {
        return Ok(RestoreOutcome::Occupied);
    }
    let restored = backend
        .restore(&snapshot.filename())
        .await
        .map_err(|error| {
            format!("KV restore failed; restart this runtime before cold inference: {error}")
        })?;
    if restored != snapshot.tokens {
        return Err("KV restore count mismatch; restart this runtime before cold inference".into());
    }
    let restored_state = backend.state().await.map_err(|error| {
        format!("KV restored slot could not be inspected; restart this runtime before cold inference: {error}")
    })?;
    if restored_state.busy
        || restored_state
            .tokens
            .map_or(!fresh_process, |tokens| tokens != snapshot.tokens)
    {
        return Err(
            "KV restored slot could not be verified; restart this runtime before cold inference"
                .into(),
        );
    }
    store.restored(&snapshot.id, now_ms)?;
    snapshot.last_used_at_ms = snapshot.last_used_at_ms.max(now_ms);
    Ok(RestoreOutcome::Restored(snapshot))
}

/// Shared administrative API implemented by current llama.cpp and Strata.
/// Enabling an adapter is separate from constructing this HTTP transport.
pub struct HttpSlotBackend {
    base: reqwest::Url,
    client: reqwest::Client,
}

impl HttpSlotBackend {
    pub fn new(endpoint: &str) -> Result<Self, String> {
        let base = reqwest::Url::parse(endpoint).map_err(|_| "Invalid KV runtime endpoint")?;
        if base.scheme() != "http"
            || base.host_str() != Some("127.0.0.1")
            || base.port().is_none()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err("KV runtime administration requires a bare 127.0.0.1 HTTP endpoint with an explicit port".into());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { base, client })
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, String> {
        // Bound the entire administrative exchange, including body consumption.
        tokio::time::timeout(Duration::from_secs(120), async {
            let mut response = request.send().await.map_err(|e| e.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("KV runtime returned HTTP {}", status.as_u16()));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                if bytes.len() + chunk.len() > 256 * 1024 {
                    return Err("KV runtime returned oversized administrative metadata".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| "Invalid KV runtime response".into())
        })
        .await
        .map_err(|_| "KV runtime operation timed out; it may still be running".to_owned())?
    }

    fn action(&self, action: &str, filename: &str) -> Result<reqwest::RequestBuilder, String> {
        let id = filename
            .strip_suffix(".bin")
            .ok_or("Invalid snapshot filename")?;
        if !uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
            return Err("Invalid snapshot filename".into());
        }
        Ok(self
            .client
            .post(self.base.join(&format!("slots/0?action={action}")).unwrap())
            .json(&serde_json::json!({"filename":filename})))
    }
}

impl SlotBackend for HttpSlotBackend {
    async fn state(&self) -> Result<SlotState, String> {
        #[derive(Deserialize)]
        struct Slot {
            id: u64,
            is_processing: bool,
            n_prompt_tokens: Option<u64>,
        }
        let slots: Vec<Slot> = self
            .json(self.client.get(self.base.join("slots").unwrap()))
            .await?;
        if slots.len() != 1 || slots[0].id != 0 {
            return Err(
                "KV persistence currently requires exactly one loaded slot (slot 0)".into(),
            );
        }
        Ok(SlotState {
            busy: slots[0].is_processing,
            tokens: slots[0].n_prompt_tokens,
        })
    }

    async fn save(&self, filename: &str) -> Result<SavedState, String> {
        #[derive(Deserialize)]
        struct Save {
            id_slot: u64,
            filename: String,
            n_saved: u64,
            n_written: u64,
        }
        let saved: Save = self.json(self.action("save", filename)?).await?;
        if saved.id_slot != 0 || saved.filename != filename {
            return Err("KV save response did not identify the requested file and slot".into());
        }
        Ok(SavedState {
            tokens: saved.n_saved,
            bytes: saved.n_written,
        })
    }

    async fn restore(&self, filename: &str) -> Result<u64, String> {
        #[derive(Deserialize)]
        struct Restore {
            id_slot: u64,
            filename: String,
            n_restored: u64,
        }
        let restored: Restore = self.json(self.action("restore", filename)?).await?;
        if restored.id_slot != 0 || restored.filename != filename {
            return Err("KV restore response did not identify the requested file and slot".into());
        }
        Ok(restored.n_restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::Mutex};
    use uuid::Uuid;

    struct Fake {
        root: PathBuf,
        state: Mutex<SlotState>,
        fail: bool,
    }
    impl SlotBackend for Fake {
        async fn state(&self) -> Result<SlotState, String> {
            Ok(*self.state.lock().unwrap())
        }
        async fn save(&self, filename: &str) -> Result<SavedState, String> {
            fs::write(self.root.join(filename), b"state").unwrap();
            if self.fail {
                return Err("simulated timeout".into());
            }
            Ok(SavedState {
                tokens: self.state.lock().unwrap().tokens.unwrap(),
                bytes: 5,
            })
        }
        async fn restore(&self, _: &str) -> Result<u64, String> {
            if self.fail {
                return Err("simulated timeout".into());
            }
            self.state.lock().unwrap().tokens = Some(100);
            Ok(100)
        }
    }
    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                root: std::env::temp_dir().join(format!("relay-kv-http-{}", Uuid::new_v4())),
            }
        }
        fn backend(&self, tokens: u64, fail: bool) -> Fake {
            Fake {
                root: self.root.clone(),
                state: Mutex::new(SlotState {
                    busy: false,
                    tokens: Some(tokens),
                }),
                fail,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn identity() -> CacheIdentity {
        CacheIdentity {
            profile: "qwen-test".into(),
            compatibility: "tested-build-and-config".into(),
        }
    }

    #[tokio::test]
    async fn save_then_restore_updates_use_only_after_verified_success() {
        let fixture = Fixture::new();
        let mut store = CacheStore::open(&fixture.root).unwrap();
        let SaveOutcome::Saved(snapshot) =
            save_idle(&mut store, &fixture.backend(100, false), &identity(), 1)
                .await
                .unwrap()
        else {
            panic!()
        };
        assert!(
            restore_empty(&mut store, &fixture.backend(0, true), &identity(), 2)
                .await
                .unwrap_err()
                .contains("restart")
        );
        assert_eq!(store.list().unwrap()[0].last_used_at_ms, 1);
        let RestoreOutcome::Restored(restored) =
            restore_empty(&mut store, &fixture.backend(0, false), &identity(), 3)
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(restored.id, snapshot.id);
        assert_eq!(store.list().unwrap()[0].last_used_at_ms, 3);
    }

    #[tokio::test]
    async fn save_timeout_retains_pending_write_and_old_snapshot() {
        let fixture = Fixture::new();
        let mut store = CacheStore::open(&fixture.root).unwrap();
        save_idle(&mut store, &fixture.backend(100, false), &identity(), 1)
            .await
            .unwrap();
        assert!(
            save_idle(&mut store, &fixture.backend(100, true), &identity(), 2)
                .await
                .is_err()
        );
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.candidate(&identity(), 3).unwrap().is_some());
        store.recover_after_runtimes_stopped().unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn active_and_empty_slots_are_not_saved_and_resident_state_is_not_replaced() {
        let fixture = Fixture::new();
        let mut store = CacheStore::open(&fixture.root).unwrap();
        let backend = fixture.backend(0, false);
        assert_eq!(
            save_idle(&mut store, &backend, &identity(), 1)
                .await
                .unwrap(),
            SaveOutcome::Empty
        );
        *backend.state.lock().unwrap() = SlotState {
            busy: true,
            tokens: Some(100),
        };
        assert_eq!(
            save_idle(&mut store, &backend, &identity(), 2)
                .await
                .unwrap(),
            SaveOutcome::Busy
        );
        assert!(store.list().unwrap().is_empty());
        save_idle(&mut store, &fixture.backend(100, false), &identity(), 3)
            .await
            .unwrap();
        assert_eq!(
            restore_empty(&mut store, &backend, &identity(), 4)
                .await
                .unwrap(),
            RestoreOutcome::Occupied
        );
        assert_eq!(
            restore_empty(&mut store, &fixture.backend(100, false), &identity(), 4)
                .await
                .unwrap(),
            RestoreOutcome::Occupied
        );
    }

    #[test]
    fn only_loopback_runtime_and_managed_filenames_are_accepted() {
        for endpoint in [
            "https://127.0.0.1:5800",
            "http://localhost:5800",
            "http://example.com:5800",
            "http://127.0.0.1:5800/v1",
            "http://user@127.0.0.1:5800",
        ] {
            assert!(HttpSlotBackend::new(endpoint).is_err());
        }
        let backend = HttpSlotBackend::new("http://127.0.0.1:5800").unwrap();
        assert!(backend.action("save", "../outside.bin").is_err());
        assert!(backend
            .action("save", &format!("{}.bin", Uuid::new_v4()))
            .is_ok());
    }

    #[tokio::test]
    async fn http_adapter_round_trip_with_restart_and_runtime_refusal() {
        use axum::{
            extract::{Query, State},
            routing::get,
            Json, Router,
        };
        use serde_json::{json, Value};
        use std::{collections::HashMap, sync::Arc};
        #[derive(Clone)]
        struct Mock {
            root: PathBuf,
            tokens: Arc<Mutex<u64>>,
            omit_token_count: Arc<std::sync::atomic::AtomicBool>,
        }
        async fn slots(State(mock): State<Mock>) -> Json<Value> {
            let mut slot = json!({"id":0,"is_processing":false});
            if !mock
                .omit_token_count
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                slot["n_prompt_tokens"] = json!(*mock.tokens.lock().unwrap());
            }
            Json(json!([slot]))
        }
        async fn action(
            State(mock): State<Mock>,
            Query(query): Query<HashMap<String, String>>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let filename = body["filename"].as_str().unwrap();
            if query["action"] == "save" {
                fs::write(mock.root.join(filename), b"state").unwrap();
                Json(json!({"id_slot":0,"filename":filename,"n_saved":100,"n_written":5}))
            } else {
                assert_eq!(fs::read(mock.root.join(filename)).unwrap(), b"state");
                *mock.tokens.lock().unwrap() = 100;
                Json(json!({"id_slot":0,"filename":filename,"n_restored":100}))
            }
        }
        let fixture = Fixture::new();
        let mut store = CacheStore::open(&fixture.root).unwrap();
        let mock = Mock {
            root: fixture.root.clone(),
            tokens: Arc::new(Mutex::new(100)),
            omit_token_count: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/slots", get(slots))
            .route("/slots/0", axum::routing::post(action))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let backend = HttpSlotBackend::new(&endpoint).unwrap();
        assert!(matches!(
            save_idle(&mut store, &backend, &identity(), 1)
                .await
                .unwrap(),
            SaveOutcome::Saved(_)
        ));
        *mock.tokens.lock().unwrap() = 0; // Fresh process, no live slot cache.
        assert!(matches!(
            restore_empty(&mut store, &backend, &identity(), 2)
                .await
                .unwrap(),
            RestoreOutcome::Restored(_)
        ));
        // Native llama.cpp omits n_prompt_tokens before any generation task,
        // even after a successful restore. Only a fresh-process owner can
        // restore in that case; an arbitrary idle runtime must be refused.
        mock.omit_token_count
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(backend.state().await.unwrap().tokens, None);
        assert_eq!(
            restore_empty(&mut store, &backend, &identity(), 3)
                .await
                .unwrap(),
            RestoreOutcome::Occupied
        );
        assert_eq!(store.list().unwrap()[0].last_used_at_ms, 2);
        assert!(matches!(
            restore_fresh(&mut store, &backend, &identity(), 4)
                .await
                .unwrap(),
            RestoreOutcome::Restored(_)
        ));
        mock.omit_token_count
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            restore_fresh(&mut store, &backend, &identity(), 5)
                .await
                .unwrap(),
            RestoreOutcome::Occupied
        );
        server.abort();
        assert!(backend.state().await.is_err());
    }
}
