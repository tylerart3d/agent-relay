//! Error notices shown in their own window beside the tray popup, so an error
//! never covers or reflows the popup's controls. Each source owns one keyed
//! notice; setting it again replaces it and clearing it removes it.
use std::sync::Mutex;

use serde::Serialize;
use tauri::{Emitter, LogicalSize, Manager, Runtime};

use crate::tray::{place_notice, NOTICE_WINDOW_LABEL};

const MAX_NOTICES: usize = 6;
const MAX_MESSAGE_CHARS: usize = 2_000;
const NOTICE_WIDTH: f64 = 360.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Notice {
    pub key: String,
    pub title: String,
    pub message: String,
    pub at_ms: i64,
}

#[derive(Default)]
pub(crate) struct Notices(Mutex<Vec<Notice>>);

impl Notices {
    /// Returns whether the visible list changed.
    pub(crate) fn set(&self, key: &str, title: &str, message: Option<&str>, now_ms: i64) -> bool {
        let mut notices = self.0.lock().expect("notices poisoned");
        let message = message
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| m.chars().take(MAX_MESSAGE_CHARS).collect::<String>());
        let existing = notices.iter().position(|n| n.key == key);
        match (message, existing) {
            (None, None) => false,
            (None, Some(index)) => {
                notices.remove(index);
                true
            }
            (Some(message), Some(index))
                if notices[index].message == message && notices[index].title == title =>
            {
                false
            }
            (Some(message), existing) => {
                if let Some(index) = existing {
                    notices.remove(index);
                }
                notices.push(Notice {
                    key: key.into(),
                    title: title.into(),
                    message,
                    at_ms: now_ms,
                });
                // Oldest notices give way so the window stays a readable size.
                while notices.len() > MAX_NOTICES {
                    notices.remove(0);
                }
                true
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<Notice> {
        self.0.lock().expect("notices poisoned").clone()
    }

    pub(crate) fn clear_all(&self) -> bool {
        let mut notices = self.0.lock().expect("notices poisoned");
        let changed = !notices.is_empty();
        notices.clear();
        changed
    }
}

/// Publish a change and show, move or hide the notice window to match.
pub(crate) fn refresh<R: Runtime>(app: &tauri::AppHandle<R>, notices: &Notices) {
    let _ = app.emit("notices-changed", ());
    let Some(window) = app.get_webview_window(NOTICE_WINDOW_LABEL) else {
        return;
    };
    if notices.list().is_empty() {
        let _ = window.hide();
        return;
    }
    let _ = place_notice(app, &window);
    let _ = window.show();
}

/// The notice page reports its content height; the window follows it.
pub(crate) fn resize<R: Runtime>(
    app: &tauri::AppHandle<R>,
    logical_height: f64,
) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window(NOTICE_WINDOW_LABEL) else {
        return Ok(());
    };
    window.set_size(LogicalSize::new(
        NOTICE_WIDTH,
        logical_height.clamp(48.0, 480.0),
    ))?;
    if window.is_visible()? {
        place_notice(app, &window)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_notice_per_source_replaced_cleared_and_bounded() {
        let notices = Notices::default();
        assert!(notices.set("tray.launch", "Launch", Some("first"), 1));
        assert!(!notices.set("tray.launch", "Launch", Some("first"), 2));
        assert!(notices.set("tray.launch", "Launch", Some("second"), 3));
        assert_eq!(notices.list().len(), 1);
        assert_eq!(notices.list()[0].message, "second");
        // Blank or missing messages clear the source's notice.
        assert!(notices.set("tray.launch", "Launch", Some("  "), 4));
        assert!(notices.list().is_empty());
        assert!(!notices.set("tray.launch", "Launch", None, 5));
        for i in 0..10 {
            notices.set(&format!("k{i}"), "T", Some("m"), i);
        }
        let list = notices.list();
        assert_eq!(list.len(), MAX_NOTICES);
        assert_eq!(list[0].key, "k4");
        assert!(notices.set("big", "T", Some(&"x".repeat(5_000)), 0));
        assert_eq!(
            notices.list().last().unwrap().message.chars().count(),
            MAX_MESSAGE_CHARS
        );
        assert!(notices.clear_all());
        assert!(!notices.clear_all());
    }
}
