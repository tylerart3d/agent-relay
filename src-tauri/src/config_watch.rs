use std::{
    collections::hash_map::DefaultHasher,
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::tray;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);
static INTERNAL_CONFIG_CHANGES: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();

#[derive(Clone, Serialize)]
struct ConfigChangedPayload {
    path: String,
}

pub async fn watch(app: AppHandle, path: PathBuf) {
    let mut previous = config_digest(&path).ok();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let Ok(candidate) = config_digest(&path) else {
            continue;
        };
        if previous == Some(candidate) {
            continue;
        }

        tokio::time::sleep(DEBOUNCE_INTERVAL).await;
        let Ok(stable) = config_digest(&path) else {
            continue;
        };
        if previous == Some(stable) {
            continue;
        }
        previous = Some(stable);
        if consume_internal_change(&path, stable) {
            continue;
        }
        if let Ok(cursor) = app.cursor_position() {
            tray::show_tray_menu(&app, cursor);
        }
        let _ = app.emit(
            "llama-swap-config-changed",
            ConfigChangedPayload {
                path: path.display().to_string(),
            },
        );
    }
}

pub(crate) fn record_internal_change(path: &Path) -> std::io::Result<()> {
    let digest = config_digest(path)?;
    let mut changes = INTERNAL_CONFIG_CHANGES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    changes.insert(path.to_path_buf(), digest);
    Ok(())
}

fn consume_internal_change(path: &Path, digest: u64) -> bool {
    let mut changes = INTERNAL_CONFIG_CHANGES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    changes.remove(path) == Some(digest)
}

fn config_digest(path: &Path) -> std::io::Result<u64> {
    let contents = fs::read(path)?;
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_changes_only_when_contents_change() {
        let path = std::env::temp_dir().join(format!(
            "agent-relay-config-watch-test-{}.yaml",
            std::process::id()
        ));
        fs::write(&path, "models: {}\n").expect("write initial config");
        let first = config_digest(&path).expect("digest initial config");
        fs::write(&path, "models: {}\n").expect("rewrite same config");
        assert_eq!(config_digest(&path).expect("digest same config"), first);
        fs::write(&path, "models:\n  qwen: {}\n").expect("write changed config");
        assert_ne!(config_digest(&path).expect("digest changed config"), first);
        fs::remove_file(path).expect("remove test config");
    }

    #[test]
    fn consumes_only_the_internal_digest_that_was_recorded() {
        let path = std::env::temp_dir().join(format!(
            "agent-relay-config-watch-internal-test-{}.yaml",
            std::process::id()
        ));
        fs::write(&path, "models:\n  qwen: {}\n").expect("write internal config");
        record_internal_change(&path).expect("record internal config");
        let internal = config_digest(&path).expect("digest internal config");
        assert!(consume_internal_change(&path, internal));
        assert!(!consume_internal_change(&path, internal));

        record_internal_change(&path).expect("record second internal config");
        fs::write(&path, "models:\n  qwen:\n    cmd: changed\n").expect("write external config");
        let external = config_digest(&path).expect("digest external config");
        assert!(!consume_internal_change(&path, external));
        fs::remove_file(path).expect("remove test config");
    }
}
