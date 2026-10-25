//! Cache adapter helper and diagnostic runner (not the public Relay CLI).
//! The caller owns start/stop and ensures exclusive access. `restore-fresh`
//! requires a newly started native process with no inference admitted yet.
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
    if args.len() != 5
        || !([
            "save",
            "restore",
            "restore-fresh",
            "list",
            "prune",
            "recover",
        ]
        .contains(&args[0].as_str())
            || args[0].starts_with("discard:"))
    {
        return Err("Usage: kv-cache-probe <save|restore|restore-fresh|list|prune|recover|discard:UUID> ROOT ENDPOINT PROFILE COMPATIBILITY_DIGEST".into());
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
        "restore" | "restore-fresh" => match if args[0] == "restore-fresh" {
            kv_cache_runtime::restore_fresh(&mut store, &backend, &identity, now).await?
        } else {
            kv_cache_runtime::restore_empty(&mut store, &backend, &identity, now).await?
        } {
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
        "recover" => {
            store.recover_after_runtimes_stopped()?;
            store.prune(now)?;
            json!({"snapshots":store.list()?})
        }
        action if action.starts_with("discard:") => {
            store.discard_quiescent(&action[8..])?;
            json!({"discarded":true})
        }
        _ => json!({"snapshots":store.list()?}),
    };
    println!(
        "{}",
        serde_json::to_string(&result).map_err(|e| e.to_string())?
    );
    Ok(())
}
