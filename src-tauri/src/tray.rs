use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, LogicalSize, Manager, PhysicalPosition, Runtime,
};

const MAIN_WINDOW_LABEL: &str = "main";
const MENU_WINDOW_LABEL: &str = "tray-menu";
const MODEL_WINDOW_LABEL: &str = "model-menu";
const CLOSE_ANIMATION_MS: u64 = 95;
const MODEL_MENU_WIDTH: f64 = 330.0;
const MODEL_MENU_MIN_HEIGHT: f64 = 160.0;
const MODEL_MENU_FALLBACK_MAX_HEIGHT: f64 = 560.0;
const MODEL_MENU_SCREEN_HEIGHT_FRACTION: f64 = 2.0 / 3.0;
static MENU_VISIBILITY_EPOCH: AtomicU64 = AtomicU64::new(0);
static TRAY_MENU_EPOCH: AtomicU64 = AtomicU64::new(0);
static MODEL_MENU_REQUEST: Mutex<Option<ModelMenuRequest>> = Mutex::new(None);

#[derive(Clone, Copy, Debug)]
struct ModelMenuRequest {
    request_id: u64,
    anchor_y: f64,
    menu_epoch: u64,
}

impl ModelMenuRequest {
    fn matches(&self, request_id: u64, menu_epoch: u64) -> bool {
        self.request_id == request_id && self.menu_epoch == menu_epoch
    }

    fn accepts_next(
        current: Option<&Self>,
        request_id: u64,
        menu_epoch: u64,
        active_menu_epoch: u64,
    ) -> bool {
        active_menu_epoch == menu_epoch
            && !current.is_some_and(|request| {
                request.menu_epoch == menu_epoch && request.request_id >= request_id
            })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct TrayMenuOpened {
    origin: &'static str,
    menu_epoch: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
struct ModelMenuOpened {
    host_id: String,
    request_id: u64,
}

pub fn setup<R: Runtime>(app: &tauri::App<R>) -> tauri::Result<()> {
    let icon = app.default_window_icon().cloned();
    let mut builder = TrayIconBuilder::with_id("agent-relay")
        .tooltip("Agent Relay")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                position,
                button: MouseButton::Left | MouseButton::Right,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_tray_menu(tray.app_handle(), position);
            }
        });
    if let Some(icon) = icon {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

pub(crate) fn show_window<R: Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub(crate) fn control_surface_visible<R: Runtime>(app: &tauri::AppHandle<R>) -> bool {
    [MAIN_WINDOW_LABEL, MENU_WINDOW_LABEL, MODEL_WINDOW_LABEL]
        .iter()
        .filter_map(|label| app.get_webview_window(label))
        .any(|window| window.is_visible().unwrap_or(false))
}

pub(crate) fn show_tray_menu<R: Runtime>(app: &tauri::AppHandle<R>, cursor: PhysicalPosition<f64>) {
    let menu_epoch = TRAY_MENU_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut request = MODEL_MENU_REQUEST
            .lock()
            .expect("model menu request poisoned");
        *request = None;
        MENU_VISIBILITY_EPOCH.fetch_add(1, Ordering::Relaxed);
    }
    let Some(window) = app.get_webview_window(MENU_WINDOW_LABEL) else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let work_area = window
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .map(|monitor| *monitor.work_area());
    let (x, y) = anchored_position(cursor, size, work_area);

    if let Some(model_menu) = app.get_webview_window(MODEL_WINDOW_LABEL) {
        let _ = model_menu.hide();
    }
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.show();
    let _ = window.set_focus();
    #[cfg(target_os = "macos")]
    let animation_origin = "top-right";
    #[cfg(not(target_os = "macos"))]
    let animation_origin = "bottom-right";
    let _ = app.emit(
        "tray-menu-opened",
        TrayMenuOpened {
            origin: animation_origin,
            menu_epoch,
        },
    );
}

pub(crate) fn show_model_menu<R: Runtime>(
    app: &tauri::AppHandle<R>,
    host_id: String,
    anchor_y: f64,
    request_id: u64,
    menu_epoch: u64,
) -> tauri::Result<()> {
    let mut request = MODEL_MENU_REQUEST
        .lock()
        .expect("model menu request poisoned");
    if !ModelMenuRequest::accepts_next(
        request.as_ref(),
        request_id,
        menu_epoch,
        TRAY_MENU_EPOCH.load(Ordering::SeqCst),
    ) {
        return Ok(());
    }
    *request = Some(ModelMenuRequest {
        request_id,
        anchor_y,
        menu_epoch,
    });
    MENU_VISIBILITY_EPOCH.fetch_add(1, Ordering::Relaxed);
    let Some(parent) = app.get_webview_window(MENU_WINDOW_LABEL) else {
        return Ok(());
    };
    let Some(window) = app.get_webview_window(MODEL_WINDOW_LABEL) else {
        return Ok(());
    };
    let parent_position = parent.outer_position()?;
    let parent_size = parent.outer_size()?;
    let scale = parent.scale_factor()?;
    let current_model_size = window.outer_size()?;
    let model_size = tauri::PhysicalSize::new(
        (MODEL_MENU_WIDTH * scale).round() as u32,
        current_model_size.height,
    );
    let monitor = parent.monitor_from_point(parent_position.x as f64, parent_position.y as f64)?;
    let work_area = monitor.map(|monitor| inset_submenu_work_area(*monitor.work_area(), scale));
    let (x, y) = submenu_position(
        parent_position,
        parent_size,
        model_size,
        anchor_y,
        scale,
        work_area,
    );

    window.set_position(PhysicalPosition::new(x, y))?;
    if TRAY_MENU_EPOCH.load(Ordering::SeqCst) != menu_epoch {
        return Ok(());
    }
    app.emit(
        "model-menu-opened",
        ModelMenuOpened {
            host_id,
            request_id,
        },
    )?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}

pub(crate) fn resize_tray_menu<R: Runtime>(
    app: &tauri::AppHandle<R>,
    logical_height: f64,
) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window(MENU_WINDOW_LABEL) else {
        return Ok(());
    };
    let height = logical_height.clamp(220.0, 600.0);
    #[cfg(not(target_os = "macos"))]
    let old_position = window.outer_position()?;
    #[cfg(not(target_os = "macos"))]
    let old_size = window.outer_size()?;
    #[cfg(not(target_os = "macos"))]
    let scale = window.scale_factor()?;
    #[cfg(not(target_os = "macos"))]
    let new_physical_height = (height * scale).round() as i32;

    window.set_size(LogicalSize::new(380.0, height))?;
    #[cfg(not(target_os = "macos"))]
    window.set_position(PhysicalPosition::new(
        old_position.x,
        old_position.y + old_size.height as i32 - new_physical_height,
    ))?;
    window.set_focus()?;
    Ok(())
}

pub(crate) fn resize_model_menu<R: Runtime>(
    app: &tauri::AppHandle<R>,
    logical_height: f64,
    request_id: u64,
) -> tauri::Result<()> {
    let request = MODEL_MENU_REQUEST
        .lock()
        .expect("model menu request poisoned");
    let Some(request) = request
        .as_ref()
        .filter(|request| request.matches(request_id, TRAY_MENU_EPOCH.load(Ordering::SeqCst)))
    else {
        return Ok(());
    };
    let Some(window) = app.get_webview_window(MODEL_WINDOW_LABEL) else {
        return Ok(());
    };
    let scale = window.scale_factor()?;
    let Some(parent) = app.get_webview_window(MENU_WINDOW_LABEL) else {
        return Ok(());
    };
    let parent_position = parent.outer_position()?;
    let parent_size = parent.outer_size()?;
    let monitor_work_area = parent
        .monitor_from_point(parent_position.x as f64, parent_position.y as f64)?
        .map(|monitor| *monitor.work_area());
    let height = model_menu_height(logical_height, scale, monitor_work_area.as_ref());
    let positioning_work_area = monitor_work_area.map(|area| inset_submenu_work_area(area, scale));
    let physical_width = (MODEL_MENU_WIDTH * scale).round() as u32;
    let physical_height = (height * scale).round() as u32;
    let (x, y) = submenu_position(
        parent_position,
        parent_size,
        tauri::PhysicalSize::new(physical_width, physical_height),
        request.anchor_y,
        scale,
        positioning_work_area,
    );

    window.set_size(LogicalSize::new(MODEL_MENU_WIDTH, height))?;
    window.set_position(PhysicalPosition::new(x, y))?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}

fn model_menu_height(
    requested_height: f64,
    scale: f64,
    work_area: Option<&tauri::PhysicalRect<i32, u32>>,
) -> f64 {
    let max_height = work_area
        .filter(|_| scale.is_finite() && scale > 0.0)
        .map(|area| (area.size.height as f64 / scale) * MODEL_MENU_SCREEN_HEIGHT_FRACTION)
        .unwrap_or(MODEL_MENU_FALLBACK_MAX_HEIGHT)
        .max(MODEL_MENU_MIN_HEIGHT);
    requested_height.clamp(MODEL_MENU_MIN_HEIGHT, max_height)
}

pub(crate) fn menus_have_focus<R: Runtime>(app: &tauri::AppHandle<R>) -> bool {
    [MENU_WINDOW_LABEL, MODEL_WINDOW_LABEL]
        .iter()
        .filter_map(|label| app.get_webview_window(label))
        .any(|window| window.is_focused().unwrap_or(false))
}

pub(crate) async fn hide_menus<R: Runtime>(app: &tauri::AppHandle<R>) {
    TRAY_MENU_EPOCH.fetch_add(1, Ordering::SeqCst);
    let close_epoch = {
        let mut request = MODEL_MENU_REQUEST
            .lock()
            .expect("model menu request poisoned");
        *request = None;
        MENU_VISIBILITY_EPOCH.fetch_add(1, Ordering::Relaxed) + 1
    };
    let _ = app.emit("tray-menus-closing", ());
    tokio::time::sleep(Duration::from_millis(CLOSE_ANIMATION_MS)).await;
    if MENU_VISIBILITY_EPOCH.load(Ordering::Relaxed) != close_epoch {
        return;
    }
    for label in [MENU_WINDOW_LABEL, MODEL_WINDOW_LABEL] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.hide();
        }
    }
}

fn anchored_position(
    cursor: PhysicalPosition<f64>,
    size: tauri::PhysicalSize<u32>,
    work_area: Option<tauri::PhysicalRect<i32, u32>>,
) -> (i32, i32) {
    let width = size.width as i32;
    let height = size.height as i32;
    let mut x = cursor.x.round() as i32 - width + 12;
    #[cfg(target_os = "macos")]
    let mut y = cursor.y.round() as i32 + 16;
    #[cfg(not(target_os = "macos"))]
    let mut y = cursor.y.round() as i32 - height - 18;

    if let Some(area) = work_area {
        let max_x = area.position.x + area.size.width as i32 - width;
        let max_y = area.position.y + area.size.height as i32 - height;
        x = x.clamp(area.position.x, max_x.max(area.position.x));
        y = y.clamp(area.position.y, max_y.max(area.position.y));
    }
    (x, y)
}

fn submenu_position(
    parent_position: PhysicalPosition<i32>,
    parent_size: tauri::PhysicalSize<u32>,
    submenu_size: tauri::PhysicalSize<u32>,
    anchor_y: f64,
    scale: f64,
    work_area: Option<tauri::PhysicalRect<i32, u32>>,
) -> (i32, i32) {
    let gap = (2.0 * scale).round() as i32;
    let submenu_width = submenu_size.width as i32;
    let submenu_height = submenu_size.height as i32;
    let left = parent_position.x - submenu_width - gap;
    let right = parent_position.x + parent_size.width as i32 + gap;
    let mut x = left;
    let y = parent_position.y + (anchor_y * scale).round() as i32;

    if let Some(area) = work_area {
        if left < area.position.x {
            x = right;
        }
        return clamp_to_work_area(
            PhysicalPosition::new(x, y),
            submenu_width,
            submenu_height,
            Some(area),
        );
    }
    (x, y)
}

fn clamp_to_work_area(
    position: PhysicalPosition<i32>,
    width: i32,
    height: i32,
    work_area: Option<tauri::PhysicalRect<i32, u32>>,
) -> (i32, i32) {
    let Some(area) = work_area else {
        return (position.x, position.y);
    };
    let max_x = area.position.x + area.size.width as i32 - width;
    let max_y = area.position.y + area.size.height as i32 - height;
    (
        position
            .x
            .clamp(area.position.x, max_x.max(area.position.x)),
        position
            .y
            .clamp(area.position.y, max_y.max(area.position.y)),
    )
}

fn inset_submenu_work_area(
    area: tauri::PhysicalRect<i32, u32>,
    scale: f64,
) -> tauri::PhysicalRect<i32, u32> {
    #[cfg(not(target_os = "macos"))]
    {
        let mut inset = area;
        inset.size.height = inset
            .size
            .height
            .saturating_sub((36.0 * scale).round() as u32);
        inset
    }
    #[cfg(target_os = "macos")]
    {
        let _ = scale;
        area
    }
}

#[cfg(test)]
mod tests {
    use tauri::{PhysicalPosition, PhysicalRect, PhysicalSize};

    use super::{anchored_position, model_menu_height, submenu_position, ModelMenuRequest};

    #[test]
    fn submenu_request_is_bound_to_its_parent_menu_epoch() {
        let request = ModelMenuRequest {
            request_id: 42,
            anchor_y: 100.0,
            menu_epoch: 7,
        };
        assert!(request.matches(42, 7));
        assert!(!request.matches(42, 8));
        assert!(!request.matches(43, 7));
    }

    #[test]
    fn submenu_requests_advance_within_one_parent_menu() {
        let host_request = ModelMenuRequest {
            request_id: 42,
            anchor_y: 100.0,
            menu_epoch: 7,
        };
        assert!(ModelMenuRequest::accepts_next(
            Some(&host_request),
            43,
            7,
            7
        ));
        assert!(!ModelMenuRequest::accepts_next(
            Some(&host_request),
            42,
            7,
            7
        ));
        assert!(!ModelMenuRequest::accepts_next(
            Some(&host_request),
            43,
            7,
            8
        ));
    }

    #[test]
    fn a_new_parent_menu_does_not_inherit_request_ordering() {
        let previous_menu = ModelMenuRequest {
            request_id: 900,
            anchor_y: 100.0,
            menu_epoch: 6,
        };
        assert!(ModelMenuRequest::accepts_next(
            Some(&previous_menu),
            1,
            7,
            7
        ));
    }

    #[test]
    fn tray_menu_position_is_clamped_to_the_work_area() {
        let work_area = PhysicalRect {
            position: PhysicalPosition::new(0, 0),
            size: PhysicalSize::new(1920, 1040),
        };
        let position = anchored_position(
            PhysicalPosition::new(1910.0, 1030.0),
            PhysicalSize::new(380, 600),
            Some(work_area),
        );
        assert_eq!(position.0, 1540);
        #[cfg(target_os = "macos")]
        assert_eq!(position.1, 440);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(position.1, 412);
    }

    #[test]
    fn submenu_prefers_the_left_side_and_aligns_to_its_host() {
        let work_area = PhysicalRect {
            position: PhysicalPosition::new(0, 0),
            size: PhysicalSize::new(1920, 1040),
        };
        assert_eq!(
            submenu_position(
                PhysicalPosition::new(1500, 400),
                PhysicalSize::new(380, 300),
                PhysicalSize::new(330, 500),
                112.0,
                1.0,
                Some(work_area),
            ),
            (1168, 512)
        );
    }

    #[test]
    fn submenu_expands_until_two_thirds_of_the_work_area() {
        let work_area = PhysicalRect {
            position: PhysicalPosition::new(0, 0),
            size: PhysicalSize::new(1920, 1080),
        };
        assert_eq!(model_menu_height(640.0, 1.0, Some(&work_area)), 640.0);
        assert_eq!(model_menu_height(900.0, 1.0, Some(&work_area)), 720.0);
    }

    #[test]
    fn submenu_height_uses_logical_pixels_on_scaled_displays() {
        let work_area = PhysicalRect {
            position: PhysicalPosition::new(0, 0),
            size: PhysicalSize::new(2560, 2160),
        };
        assert_eq!(model_menu_height(800.0, 1.5, Some(&work_area)), 800.0);
        assert_eq!(model_menu_height(1200.0, 1.5, Some(&work_area)), 960.0);
    }
}
