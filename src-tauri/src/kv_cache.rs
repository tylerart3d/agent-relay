//! Host-local disk snapshot retention. Runtime adapters own serialization and
//! must prove cross-process prefix reuse before enabling automatic persistence.
//!
//! This is deliberately separate from telemetry: snapshots contain private
//! conversation state. The catalog holds no messages or conversation identifiers.
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use rusqlite::{params, Connection};
use serde::Serialize;
use uuid::Uuid;

pub const MAX_SNAPSHOTS: usize = 3;
pub const UNUSED_RETENTION_MS: u64 = 10 * 24 * 60 * 60 * 1_000;

/// Opaque digests supplied by the adapter. Include model/projector/draft bytes,
/// runtime build and effective cache-affecting settings (context, KV types,
/// RoPE, template). Never use a model display name as the compatibility key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheIdentity {
    pub profile: String,
    pub compatibility: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub id: String,
    pub profile: String,
    pub compatibility: String,
    pub saved_at_ms: u64,
    pub last_used_at_ms: u64,
    pub tokens: u64,
    pub bytes: u64,
    pub ready: bool,
}

impl Snapshot {
    /// The sole filename passed to a runtime's slot save/restore endpoint.
    pub fn filename(&self) -> String {
        format!("{}.bin", self.id)
    }
}

/// One store per host, shared by all model profiles. An OS lock excludes other
/// processes; callers serialize operations on the owner rather than sharing the
/// directory with independent helpers. Pending writes consume the same quota.
pub struct CacheStore {
    root: PathBuf,
    db: Connection,
    _lock: File,
}

fn failure(error: impl std::fmt::Display) -> String {
    format!("KV snapshot store: {error}")
}

fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // reparse points, including junctions
    }
    #[cfg(not(windows))]
    false
}

fn reject_link(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => Err("KV snapshot path must not be a link".into()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(failure(error)),
    }
}

impl CacheStore {
    pub fn open(root: &Path) -> Result<Self, String> {
        reject_link(root)?;
        let existed = root.exists();
        fs::create_dir_all(root).map_err(failure)?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(failure)?;
        }
        #[cfg(not(unix))]
        let _ = existed; // Windows inherits the user's private app-data ACL.
        let root = root.canonicalize().map_err(failure)?;
        let lock_path = root.join("store.lock");
        reject_link(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(failure)?;
        lock.try_lock()
            .map_err(|_| "KV snapshot store is already in use".to_owned())?;
        let db_path = root.join("catalog.sqlite");
        reject_link(&db_path)?;
        let db = Connection::open(db_path).map_err(failure)?;
        db.execute_batch(
            "PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS snapshots (
                id TEXT PRIMARY KEY, profile TEXT NOT NULL, compatibility TEXT NOT NULL,
                saved_at_ms INTEGER NOT NULL, last_used_at_ms INTEGER NOT NULL,
                tokens INTEGER NOT NULL, bytes INTEGER NOT NULL, ready INTEGER NOT NULL
            );",
        )
        .map_err(failure)?;
        let store = Self {
            root,
            db,
            _lock: lock,
        };
        // Validate identifiers before any filesystem operation, including after
        // a corrupted catalog. Never follow paths supplied by a runtime response.
        store.list()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self) -> Result<Vec<Snapshot>, String> {
        let mut statement = self
            .db
            .prepare(
                "SELECT id,profile,compatibility,saved_at_ms,last_used_at_ms,tokens,bytes,ready
             FROM snapshots ORDER BY last_used_at_ms ASC,saved_at_ms ASC,id ASC",
            )
            .map_err(failure)?;
        let rows = statement
            .query_map([], |row| {
                Ok(Snapshot {
                    id: row.get(0)?,
                    profile: row.get(1)?,
                    compatibility: row.get(2)?,
                    saved_at_ms: row.get(3)?,
                    last_used_at_ms: row.get(4)?,
                    tokens: row.get(5)?,
                    bytes: row.get(6)?,
                    ready: row.get(7)?,
                })
            })
            .map_err(failure)?;
        let snapshots: Vec<Snapshot> = rows.collect::<Result<_, _>>().map_err(failure)?;
        for snapshot in &snapshots {
            validate_id(&snapshot.id)?;
        }
        Ok(snapshots)
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        validate_id(id)?;
        let path = self.root.join(format!("{id}.bin"));
        reject_link(&path)?;
        Ok(path)
    }

    fn remove(&mut self, id: &str) -> Result<(), String> {
        let path = self.path(id)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(failure(error)),
        }
        self.db
            .execute("DELETE FROM snapshots WHERE id=?1", [id])
            .map_err(failure)?;
        Ok(())
    }

    /// Run at startup, before cache operations, and hourly while Relay is idle.
    /// Pending files are never deleted here: an HTTP timeout does not prove the
    /// runtime stopped writing. Recovery requires confirmed runtime shutdown.
    pub fn prune(&mut self, now_ms: u64) -> Result<(), String> {
        for snapshot in self.list()? {
            if snapshot.ready
                && now_ms.saturating_sub(snapshot.last_used_at_ms) >= UNUSED_RETENTION_MS
            {
                self.remove(&snapshot.id)?;
            }
        }
        Ok(())
    }

    /// Reserve capacity *before* the runtime starts writing. Keep the previous
    /// snapshot for this profile when possible, so a failed replacement leaves
    /// it usable. At most three ready + pending snapshots can exist at once.
    pub fn reserve(&mut self, identity: &CacheIdentity, now_ms: u64) -> Result<Snapshot, String> {
        if identity.profile.is_empty() || identity.compatibility.is_empty() {
            return Err("KV snapshot identity must be explicit".into());
        }
        self.prune(now_ms)?;
        let snapshots = self.list()?;
        if snapshots.iter().any(|snapshot| !snapshot.ready) {
            return Err(
                "A KV save is pending; confirm runtime shutdown before discarding it".into(),
            );
        }
        if snapshots.len() >= MAX_SNAPSHOTS {
            let victim = snapshots
                .iter()
                .find(|entry| entry.profile != identity.profile)
                .or_else(|| snapshots.first())
                .expect("nonempty snapshot list");
            self.remove(&victim.id)?;
        }
        let snapshot = Snapshot {
            id: Uuid::new_v4().to_string(),
            profile: identity.profile.clone(),
            compatibility: identity.compatibility.clone(),
            saved_at_ms: now_ms,
            last_used_at_ms: now_ms,
            tokens: 0,
            bytes: 0,
            ready: false,
        };
        self.db
            .execute(
                "INSERT INTO snapshots VALUES (?1,?2,?3,?4,?4,0,0,0)",
                params![
                    snapshot.id,
                    snapshot.profile,
                    snapshot.compatibility,
                    now_ms
                ],
            )
            .map_err(failure)?;
        Ok(snapshot)
    }

    /// Publish only after a complete successful runtime response. The runtime
    /// writes into the private store root and must flush its output itself.
    pub fn commit(
        &mut self,
        id: &str,
        tokens: u64,
        reported_bytes: u64,
    ) -> Result<Snapshot, String> {
        let mut snapshot = self
            .list()?
            .into_iter()
            .find(|entry| entry.id == id && !entry.ready)
            .ok_or("Unknown pending KV snapshot")?;
        let metadata = fs::symlink_metadata(self.path(id)?).map_err(failure)?;
        if !metadata.is_file()
            || metadata.len() != reported_bytes
            || reported_bytes == 0
            || tokens == 0
        {
            return Err("KV snapshot save did not produce a complete, nonempty state file".into());
        }
        // fsync the completed file before publishing metadata. Runtime adapters
        // must also validate save responses, never treating HTTP 200 as sufficient.
        OpenOptions::new()
            .read(true)
            .write(true) // FlushFileBuffers requires write access on Windows.
            .open(self.path(id)?)
            .and_then(|file| file.sync_all())
            .map_err(failure)?;
        self.db
            .execute(
                "UPDATE snapshots SET ready=1,tokens=?2,bytes=?3 WHERE id=?1",
                params![id, tokens, reported_bytes],
            )
            .map_err(failure)?;
        snapshot.ready = true;
        snapshot.tokens = tokens;
        snapshot.bytes = reported_bytes;
        for old in self.list()? {
            if old.id != id && old.profile == snapshot.profile {
                self.remove(&old.id)?;
            }
        }
        Ok(snapshot)
    }

    /// Selection does not refresh expiry: failed attempts must not keep stale
    /// state alive. Adapters must treat a miss as an ordinary cold prefill.
    pub fn candidate(
        &mut self,
        identity: &CacheIdentity,
        now_ms: u64,
    ) -> Result<Option<Snapshot>, String> {
        self.prune(now_ms)?;
        for snapshot in self.list()?.into_iter().rev() {
            if snapshot.ready
                && snapshot.profile == identity.profile
                && snapshot.compatibility == identity.compatibility
            {
                let valid = fs::symlink_metadata(self.path(&snapshot.id)?)
                    .is_ok_and(|meta| meta.is_file() && meta.len() == snapshot.bytes);
                if valid {
                    return Ok(Some(snapshot));
                }
                self.remove(&snapshot.id)?;
            }
        }
        Ok(None)
    }

    /// Call only after the runtime confirms successful restoration. This is the
    /// only read operation that refreshes the ten-day lifetime and LRU ordering.
    pub fn restored(&mut self, id: &str, now_ms: u64) -> Result<(), String> {
        validate_id(id)?;
        if self
            .db
            .execute(
                "UPDATE snapshots SET last_used_at_ms=max(last_used_at_ms,?2)
            WHERE id=?1 AND ready=1",
                params![id, now_ms],
            )
            .map_err(failure)?
            != 1
        {
            return Err("Unknown ready KV snapshot".into());
        }
        Ok(())
    }

    /// Invalid files may be discarded after a completed restore refusal, but
    /// never while a native save/restore operation may still hold the file.
    pub fn discard_quiescent(&mut self, id: &str) -> Result<(), String> {
        self.remove(id)
    }

    /// Explicit recovery barrier. Call only after all cache-writing runtimes
    /// are confirmed stopped, including adopted children from the previous app.
    pub fn recover_after_runtimes_stopped(&mut self) -> Result<(), String> {
        for snapshot in self.list()? {
            if !snapshot.ready {
                self.remove(&snapshot.id)?;
            }
        }
        let known: Vec<String> = self.list()?.iter().map(Snapshot::filename).collect();
        for entry in fs::read_dir(&self.root).map_err(failure)? {
            let entry = entry.map_err(failure)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".bin") {
                if validate_id(id).is_ok() && !known.contains(&name) {
                    // Only our exact UUID namespace, never arbitrary user files.
                    fs::remove_file(self.path(id)?).map_err(failure)?;
                }
            }
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
        Ok(())
    } else {
        Err("Invalid KV snapshot identifier".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                root: std::env::temp_dir().join(format!("relay-kv-{}", Uuid::new_v4())),
            }
        }
        fn open(&self) -> CacheStore {
            CacheStore::open(&self.root).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn identity(profile: &str) -> CacheIdentity {
        CacheIdentity {
            profile: profile.into(),
            compatibility: "test-build-model-context-digest".into(),
        }
    }
    fn save(store: &mut CacheStore, profile: &str, time: u64) -> Snapshot {
        let entry = store.reserve(&identity(profile), time).unwrap();
        fs::write(store.root().join(entry.filename()), b"synthetic-state").unwrap();
        store.commit(&entry.id, 1000, 15).unwrap()
    }

    #[test]
    fn three_snapshots_total_with_lru_eviction_before_fourth_save() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let a = save(&mut store, "a", 1);
        let b = save(&mut store, "b", 2);
        save(&mut store, "c", 3);
        store.restored(&a.id, 4).unwrap();
        let d = store.reserve(&identity("d"), 5).unwrap();
        assert_eq!(store.list().unwrap().len(), 3);
        assert!(!store.root().join(b.filename()).exists());
        assert!(store.reserve(&identity("e"), 6).is_err());
        assert!(!d.ready);
    }

    #[test]
    fn expiry_is_ten_days_since_successful_use_and_survives_restart() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let a = save(&mut store, "a", 1);
        store.restored(&a.id, 1000).unwrap();
        drop(store);
        let mut store = fixture.open();
        store.prune(UNUSED_RETENTION_MS + 999).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        store.prune(UNUSED_RETENTION_MS + 1000).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(!store.root().join(a.filename()).exists());
    }

    #[test]
    fn lookup_and_failed_restores_do_not_refresh_expiry() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        save(&mut store, "a", 1);
        assert!(store.candidate(&identity("a"), 1000).unwrap().is_some());
        assert!(store
            .candidate(&identity("a"), UNUSED_RETENTION_MS + 1)
            .unwrap()
            .is_none());
    }

    #[test]
    fn failed_replacement_preserves_previous_and_pending_requires_shutdown() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let a = save(&mut store, "a", 1);
        save(&mut store, "b", 2);
        save(&mut store, "c", 3);
        let pending = store.reserve(&identity("a"), 4).unwrap();
        fs::write(store.root().join(pending.filename()), b"partial").unwrap();
        assert!(store.commit(&pending.id, 1000, 2000).is_err());
        assert_eq!(
            store.candidate(&identity("a"), 5).unwrap().unwrap().id,
            a.id
        );
        store.prune(UNUSED_RETENTION_MS + 10).unwrap();
        assert!(store.root().join(pending.filename()).exists());
        drop(store);
        let mut store = fixture.open();
        assert!(store.reserve(&identity("b"), 6).is_err());
        store.recover_after_runtimes_stopped().unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn published_replacement_removes_previous_profile_version() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let old = save(&mut store, "a", 1);
        let new = save(&mut store, "a", 2);
        assert_eq!(store.list().unwrap(), vec![new]);
        assert!(!store.root().join(old.filename()).exists());
    }

    #[test]
    fn incompatible_runtime_or_configuration_is_a_miss() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        save(&mut store, "a", 1);
        let mut changed = identity("a");
        changed.compatibility.push_str("-different-context");
        assert!(store.candidate(&changed, 2).unwrap().is_none());
        assert!(store.candidate(&identity("a"), 2).unwrap().is_some());
    }

    #[test]
    fn missing_or_truncated_state_is_not_restored() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let a = save(&mut store, "a", 1);
        fs::write(store.root().join(a.filename()), b"short").unwrap();
        assert!(store.candidate(&identity("a"), 2).unwrap().is_none());
        let a = save(&mut store, "a", 3);
        fs::remove_file(store.root().join(a.filename())).unwrap();
        assert!(store.candidate(&identity("a"), 4).unwrap().is_none());
    }

    #[test]
    fn second_owner_is_excluded_and_lock_released_on_drop() {
        let fixture = Fixture::new();
        let store = fixture.open();
        assert!(CacheStore::open(&fixture.root).is_err());
        drop(store);
        assert!(CacheStore::open(&fixture.root).is_ok());
    }

    #[test]
    fn rejects_paths_and_only_cleans_owned_orphans() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        assert!(store.discard_quiescent("../unrelated").is_err());
        fs::write(store.root().join("user.bin"), b"keep").unwrap();
        let orphan = format!("{}.bin", Uuid::new_v4());
        fs::write(store.root().join(&orphan), b"orphan").unwrap();
        store.recover_after_runtimes_stopped().unwrap();
        assert!(store.root().join("user.bin").exists());
        assert!(!store.root().join(orphan).exists());
    }

    #[test]
    fn clock_rollback_does_not_expire_or_reduce_last_use() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let a = save(&mut store, "a", 1000);
        store.restored(&a.id, 100).unwrap();
        store.prune(10).unwrap();
        assert_eq!(store.list().unwrap()[0].last_used_at_ms, 1000);
    }
}
