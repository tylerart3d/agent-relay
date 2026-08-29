use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::tray;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);

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
}
