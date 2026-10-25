//! Diagnostic runner for the persistence core, not an installed Relay command.
//! The operator owns runtime start/stop and must ensure exclusive idle access.
use agent_relay_lib::{
    kv_cache::{CacheIdentity, CacheStore},
    kv_cache_runtime::{self, HttpSlotBackend, RestoreOutcome, SaveOutcome},
};
use serde_json::json;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[tokio::main]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 5 || !["save", "restore", "list", "prune"].contains(&args[0].as_str()) {
        return Err("Usage: kv-cache-probe <save|restore|list|prune> ROOT ENDPOINT PROFILE COMPATIBILITY_DIGEST".into());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    let mut store = CacheStore::open(Path::new(&args[1]))?;
    let identity = CacheIdentity {
        profile: args[3].clone(),
        compatibility: args[4].clone(),
    };
    let backend = HttpSlotBackend::new(&args[2])?;
    let result = match args[0].as_str() {
        "save" => match kv_cache_runtime::save_idle(&mut store, &backend, &identity, now).await? {
            SaveOutcome::Saved(snapshot) => json!({"state":"saved","snapshot":snapshot}),
            SaveOutcome::Empty => return Err("No populated slot to save".into()),
            SaveOutcome::Busy => return Err("Runtime is busy; no snapshot saved".into()),
        },
        "restore" => match kv_cache_runtime::restore_empty(&mut store, &backend, &identity, now)
            .await?
        {
            RestoreOutcome::Restored(snapshot) => json!({"state":"restored","snapshot":snapshot}),
            RestoreOutcome::Miss => return Err("No compatible unexpired snapshot".into()),
            RestoreOutcome::Occupied => {
                return Err("Runtime slot is busy or populated; refusing to overwrite it".into())
            }
        },
        "prune" => {
            store.prune(now)?;
            json!({"snapshots":store.list()?})
        }
        _ => json!({"snapshots":store.list()?}),
    };
    println!(
        "{}",
        serde_json::to_string(&result).map_err(|e| e.to_string())?
    );
    Ok(())
}
