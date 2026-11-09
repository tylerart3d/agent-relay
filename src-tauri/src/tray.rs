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
pub(crate) const NOTICE_WINDOW_LABEL: &str = "notice";
const TRAY_ID: &str = "agent-relay";
/// Gap between the popup and the screen edge, in logical pixels. Windows 11
/// flyouts sit 12 px from the taskbar; macOS menus hang just under the menu bar.
#[cfg(target_os = "macos")]
const DOCK_MARGIN: f64 = 6.0;
#[cfg(not(target_os = "macos"))]
const DOCK_MARGIN: f64 = 12.0;
const NOTICE_GAP: f64 = 8.0;
static LAST_DOCK: Mutex<Option<DockContext>> = Mutex::new(None);

/// The screen edge the popup docks against: the taskbar or menu-bar edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockEdge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
struct DockContext {
    edge: DockEdge,
    work_area: tauri::PhysicalRect<i32, u32>,
    /// Horizontal center of the tray icon; macOS hangs the popup under it.
    icon_center_x: Option<i32>,
    center_on_icon: bool,
    margin: i32,
}
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

/// Opens the popup docked at the tray corner. The pointer position is used only
/// to pick a monitor when the tray icon's own location is unavailable.
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
    let context = dock_context(app, &window, cursor);
    if let Some(context) = context {
        *LAST_DOCK.lock().expect("dock context poisoned") = Some(context);
    }
    let (x, y) = match context {
        Some(context) => dock_position(&context, size),
        None => (cursor.x.round() as i32, cursor.y.round() as i32),
    };

    if let Some(model_menu) = app.get_webview_window(MODEL_WINDOW_LABEL) {
        let _ = model_menu.hide();
    }
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.show();
    let _ = window.set_focus();
    reposition_notice(app);
    let animation_origin = if context.is_some_and(|c| c.edge == DockEdge::Top) {
        "top-right"
    } else {
        "bottom-right"
    };
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
    let scale = window.scale_factor()?;
    let new_size = tauri::PhysicalSize::new(
        (380.0 * scale).round() as u32,
        (height * scale).round() as u32,
    );
    let context = *LAST_DOCK.lock().expect("dock context poisoned");

    window.set_size(LogicalSize::new(380.0, height))?;
    // Stay docked: the edge-side corner holds still while the popup grows away from it.
    if let Some(context) = context {
        let (x, y) = dock_position(&context, new_size);
        window.set_position(PhysicalPosition::new(x, y))?;
    }
    window.set_focus()?;
    reposition_notice(app);
    Ok(())
}

/// Work out the docking edge and tray icon location for the monitor holding the icon.
fn dock_context<R: Runtime>(
    app: &tauri::AppHandle<R>,
    window: &tauri::WebviewWindow<R>,
    fallback: PhysicalPosition<f64>,
) -> Option<DockContext> {
    let scale = window.scale_factor().unwrap_or(1.0);
    let icon = app
        .tray_by_id(TRAY_ID)
        .and_then(|tray| tray.rect().ok().flatten())
        .map(|rect| {
            let position = rect.position.to_physical::<f64>(scale);
            let size = rect.size.to_physical::<f64>(scale);
            PhysicalPosition::new(
                position.x + size.width / 2.0,
                position.y + size.height / 2.0,
            )
        });
    let anchor = icon.unwrap_or(fallback);
    let monitor = window
        .monitor_from_point(anchor.x, anchor.y)
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())?;
    let monitor_rect = tauri::PhysicalRect {
        position: *monitor.position(),
        size: *monitor.size(),
    };
    let work_area = *monitor.work_area();
    Some(DockContext {
        edge: dock_edge(monitor_rect, work_area),
        work_area,
        icon_center_x: icon.map(|point| point.x.round() as i32),
        center_on_icon: cfg!(target_os = "macos"),
        margin: (DOCK_MARGIN * monitor.scale_factor()).round() as i32,
    })
}

/// The taskbar or menu bar is wherever the work area stops short of the monitor.
fn dock_edge(
    monitor: tauri::PhysicalRect<i32, u32>,
    work: tauri::PhysicalRect<i32, u32>,
) -> DockEdge {
    let monitor_bottom = monitor.position.y + monitor.size.height as i32;
    let monitor_right = monitor.position.x + monitor.size.width as i32;
    let work_bottom = work.position.y + work.size.height as i32;
    let work_right = work.position.x + work.size.width as i32;
    if work.position.y > monitor.position.y {
        DockEdge::Top
    } else if work_bottom < monitor_bottom {
        DockEdge::Bottom
    } else if work.position.x > monitor.position.x {
        DockEdge::Left
    } else if work_right < monitor_right {
        DockEdge::Right
    } else if cfg!(target_os = "macos") {
        DockEdge::Top
    } else {
        // An auto-hidden taskbar leaves no gap; Windows defaults to the bottom.
        DockEdge::Bottom
    }
}

/// Fixed spot for a window of `size`: the tray corner of the work area, or on
/// macOS hanging under the menu-bar icon. Never depends on the pointer.
fn dock_position(context: &DockContext, size: tauri::PhysicalSize<u32>) -> (i32, i32) {
    let area = context.work_area;
    let margin = context.margin;
    let width = size.width as i32;
    let height = size.height as i32;
    let left = area.position.x + margin;
    let right = (area.position.x + area.size.width as i32 - width - margin).max(left);
    let top = area.position.y + margin;
    let bottom = (area.position.y + area.size.height as i32 - height - margin).max(top);
    let x = match (context.edge, context.icon_center_x) {
        (DockEdge::Left, _) => left,
        (DockEdge::Top, Some(center)) if context.center_on_icon => {
            (center - width / 2).clamp(left, right)
        }
        _ => right,
    };
    let y = if context.edge == DockEdge::Top {
        top
    } else {
        bottom
    };
    (x, y)
}

/// Notices sit beside the popup on the side away from the taskbar, or take
/// the popup's docked spot when the popup is closed.
fn notice_position(
    context: &DockContext,
    notice: tauri::PhysicalSize<u32>,
    menu: Option<(PhysicalPosition<i32>, tauri::PhysicalSize<u32>)>,
    gap: i32,
) -> (i32, i32) {
    let Some((menu_position, menu_size)) = menu else {
        return dock_position(context, notice);
    };
    let x = menu_position.x + menu_size.width as i32 - notice.width as i32;
    let y = if context.edge == DockEdge::Top {
        menu_position.y + menu_size.height as i32 + gap
    } else {
        menu_position.y - notice.height as i32 - gap
    };
    clamp_to_work_area(
        PhysicalPosition::new(x, y),
        notice.width as i32,
        notice.height as i32,
        Some(context.work_area),
    )
}

pub(crate) fn reposition_notice<R: Runtime>(app: &tauri::AppHandle<R>) {
    let Some(notice) = app.get_webview_window(NOTICE_WINDOW_LABEL) else {
        return;
    };
    if !notice.is_visible().unwrap_or(false) {
        return;
    }
    let _ = place_notice(app, &notice);
}

/// Position the notice window. Called before showing it and after any resize.
pub(crate) fn place_notice<R: Runtime>(
    app: &tauri::AppHandle<R>,
    notice: &tauri::WebviewWindow<R>,
) -> tauri::Result<()> {
    let menu = app
        .get_webview_window(MENU_WINDOW_LABEL)
        .filter(|menu| menu.is_visible().unwrap_or(false));
    let context = match *LAST_DOCK.lock().expect("dock context poisoned") {
        Some(context) => Some(context),
        None => dock_context(app, notice, PhysicalPosition::new(0.0, 0.0)),
    };
    let Some(context) = context else {
        return Ok(());
    };
    let menu_frame = match &menu {
        Some(menu) => Some((menu.outer_position()?, menu.outer_size()?)),
        None => None,
    };
    let gap = (NOTICE_GAP * notice.scale_factor()?).round() as i32;
    let (x, y) = notice_position(&context, notice.outer_size()?, menu_frame, gap);
    notice.set_position(PhysicalPosition::new(x, y))
}

/// Windows 11 rounds undecorated windows only when asked. Native rounding clips
/// the corners outright, so no transparent pixels can show through as black.
pub(crate) fn apply_native_corners<R: Runtime>(app: &tauri::AppHandle<R>) {
    #[cfg(windows)]
    for label in [MENU_WINDOW_LABEL, MODEL_WINDOW_LABEL, NOTICE_WINDOW_LABEL] {
        use windows_sys::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        };
        let Some(window) = app.get_webview_window(label) else {
            continue;
        };
        let Ok(hwnd) = window.hwnd() else {
            continue;
        };
        let preference = DWMWCP_ROUND;
        // Fails harmlessly on Windows 10, which has no rounded-corner attribute.
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as _,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                (&preference as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = app;
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
    // Clicking Dismiss on a notice must not close the popup it belongs to.
    [MENU_WINDOW_LABEL, MODEL_WINDOW_LABEL, NOTICE_WINDOW_LABEL]
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

    use super::{
        dock_edge, dock_position, model_menu_height, notice_position, submenu_position,
        DockContext, DockEdge, ModelMenuRequest,
    };

    fn rect(x: i32, y: i32, width: u32, height: u32) -> PhysicalRect<i32, u32> {
        PhysicalRect {
            position: PhysicalPosition::new(x, y),
            size: PhysicalSize::new(width, height),
        }
    }

    fn context(edge: DockEdge, work: PhysicalRect<i32, u32>) -> DockContext {
        DockContext {
            edge,
            work_area: work,
            icon_center_x: Some(1800),
            center_on_icon: false,
            margin: 12,
        }
    }

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
    fn taskbar_edge_is_where_the_work_area_stops_short() {
        let monitor = rect(0, 0, 1920, 1080);
        assert_eq!(dock_edge(monitor, rect(0, 0, 1920, 1032)), DockEdge::Bottom);
        assert_eq!(dock_edge(monitor, rect(0, 48, 1920, 1032)), DockEdge::Top);
        assert_eq!(dock_edge(monitor, rect(62, 0, 1858, 1080)), DockEdge::Left);
        assert_eq!(dock_edge(monitor, rect(0, 0, 1858, 1080)), DockEdge::Right);
        // macOS: the menu bar and a bottom Dock both shrink the work area; the menu bar wins.
        assert_eq!(dock_edge(monitor, rect(0, 25, 1920, 980)), DockEdge::Top);
        // A second monitor to the right keeps its own coordinates.
        assert_eq!(
            dock_edge(rect(1920, 0, 2560, 1440), rect(1920, 0, 2560, 1392)),
            DockEdge::Bottom
        );
    }

    #[test]
    fn popup_docks_at_the_tray_corner_regardless_of_pointer() {
        let size = PhysicalSize::new(380, 600);
        let bottom = context(DockEdge::Bottom, rect(0, 0, 1920, 1032));
        assert_eq!(dock_position(&bottom, size), (1528, 420));
        let top = context(DockEdge::Top, rect(0, 48, 1920, 1032));
        assert_eq!(dock_position(&top, size), (1528, 60));
        let left = context(DockEdge::Left, rect(62, 0, 1858, 1080));
        assert_eq!(dock_position(&left, size), (74, 468));
        let right = context(DockEdge::Right, rect(0, 0, 1858, 1080));
        assert_eq!(dock_position(&right, size), (1466, 468));
    }

    #[test]
    fn macos_popup_hangs_under_its_menu_bar_icon_and_stays_on_screen() {
        let size = PhysicalSize::new(760, 1200);
        let mut mac = context(DockEdge::Top, rect(0, 50, 3024, 1914));
        mac.center_on_icon = true;
        mac.margin = 12;
        mac.icon_center_x = Some(2400);
        assert_eq!(dock_position(&mac, size), (2020, 62));
        mac.icon_center_x = Some(2990);
        assert_eq!(dock_position(&mac, size), (2252, 62));
        mac.icon_center_x = None;
        assert_eq!(dock_position(&mac, size), (2252, 62));
    }

    #[test]
    fn growing_popup_keeps_its_taskbar_corner_fixed() {
        let bottom = context(DockEdge::Bottom, rect(0, 0, 1920, 1032));
        let short = dock_position(&bottom, PhysicalSize::new(380, 300));
        let tall = dock_position(&bottom, PhysicalSize::new(380, 600));
        assert_eq!(short.0, tall.0);
        assert_eq!(short.1 + 300, tall.1 + 600);
    }

    #[test]
    fn notice_sits_beside_the_popup_away_from_the_taskbar() {
        let notice = PhysicalSize::new(360, 120);
        let menu = (
            PhysicalPosition::new(1528, 420),
            PhysicalSize::new(380, 600),
        );
        let bottom = context(DockEdge::Bottom, rect(0, 0, 1920, 1032));
        assert_eq!(notice_position(&bottom, notice, Some(menu), 8), (1548, 292));
        let top = context(DockEdge::Top, rect(0, 48, 1920, 1032));
        let top_menu = (PhysicalPosition::new(1528, 60), PhysicalSize::new(380, 600));
        assert_eq!(
            notice_position(&top, notice, Some(top_menu), 8),
            (1548, 668)
        );
        // Without the popup the notice takes the docked spot itself.
        assert_eq!(notice_position(&bottom, notice, None, 8), (1548, 900));
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
