//! rs-sc — screenshot → box-select → multimodal AI translate → floating window.
//!
//! Hotkey and LLM config live in the settings panel; persisted to JSON under the
//! OS app data directory. Hot-swap of the active config is supported — saving new
//! values from the settings panel updates the in-memory LLM client and
//! re-registers the global hotkey without restarting the process.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod capture_window;
mod error;
mod llm_translate;
mod models;
mod settings;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use error::{AppError, AppResult};
use llm_translate::{LlmConfig, LlmTranslateClient};
use models::CaptureRect;
use settings::Settings;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tokio::sync::{Mutex, RwLock};

use crate::capture_window::CaptureEvent;

const RESULT_WINDOW_LABEL: &str = "result";
const SETTINGS_WINDOW_LABEL: &str = "settings";
const RESULT_LOADED_EVENT: &str = "result:loaded";
const RESULT_BUSY_EVENT: &str = "result:busy";
const RESULT_ERROR_EVENT: &str = "result:error";

// ── Shared application state ──────────────────────────────────────────────────

/// Per-capture state that the hotkey action installs. The atomic flag and
/// the winit close flag are flipped by the NEXT hotkey press to cancel the
/// current capture mid-flight. Each pipeline task holds one of these for
/// its entire lifetime and is expected to drop it on exit.
pub struct ActiveCapture {
    /// Set by a subsequent hotkey press to tell this capture "you're done".
    /// Checked at every await point in `pipeline_inner`.
    pub cancel: Arc<AtomicBool>,
}

impl ActiveCapture {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal the owning pipeline to wind down. Idempotent.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        // Also force-close the winit overlay so the user sees the dim screen
        // disappear immediately. The pipeline's pending mpsc recv() will block
        // until the winit handler emits a Cancelled event OR the channel is
        // dropped — we use cancel_capture() which doesn't send the event (see
        // CaptureCommand::Cancel handler) so the pipeline just notices the
        // channel closed and exits.
        crate::capture_window::cancel_capture();
    }
}

#[derive(Clone)]
struct AppState {
    llm: Arc<LlmTranslateClient>,
    settings_path: PathBuf,
    /// The hotkey currently registered with the OS. We track it explicitly so
    /// save_settings can diff old vs new and unregister the right thing.
    current_hotkey: Arc<RwLock<String>>,
    current_settings_hotkey: Arc<RwLock<String>>,
    /// The single in-flight capture, if any. Replacing this value (a new
    /// hotkey press) is how we cancel the previous one — see `cancel()`.
    active_capture: Arc<Mutex<Option<ActiveCapture>>>,
}

impl AppState {
    fn new(
        llm: LlmTranslateClient,
        settings_path: PathBuf,
        hotkey: String,
        settings_hotkey: String,
    ) -> Self {
        Self {
            llm: Arc::new(llm),
            settings_path,
            current_hotkey: Arc::new(RwLock::new(hotkey)),
            current_settings_hotkey: Arc::new(RwLock::new(settings_hotkey)),
            active_capture: Arc::new(Mutex::new(None)),
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second launch: bring the result window forward (or the settings
            // window if that's what's open).
            for label in [RESULT_WINDOW_LABEL, SETTINGS_WINDOW_LABEL] {
                if let Some(w) = app.get_webview_window(label) {
                    if w.is_visible().unwrap_or(false) {
                        let _ = w.unminimize();
                        let _ = w.show();
                        let _ = w.set_focus();
                        return;
                    }
                }
            }
            if let Some(w) = app.get_webview_window(RESULT_WINDOW_LABEL) {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            // ── Resolve app data dir and load settings ────────────────────────
            let app_data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("rs-sc"));
            let settings = Settings::load(&app_data_dir)?;

            // ── Build LLM client with the loaded config ──────────────────────
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|e| AppError::Capture(format!("reqwest build failed: {e}")))?;
            let llm = LlmTranslateClient::new(Arc::new(http), settings.into_llm_config());

            let state = AppState::new(
                llm,
                app_data_dir,
                settings.hotkey.clone(),
                settings.settings_hotkey.clone(),
            );
            app.manage(state.clone());

            // ── Wire close-to-hide on the secondary windows ───────────────────
            for label in [RESULT_WINDOW_LABEL, SETTINGS_WINDOW_LABEL] {
                if let Some(window) = app.get_webview_window(label) {
                    let w = window.clone();
                    window.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            api.prevent_close();
                            let _ = w.hide();
                        }
                    });
                }
            }

            // ── Register global hotkeys ──────────────────────────────────────
            // Two hotkeys: one for capture (the main workflow), one for opening
            // settings (a guaranteed way in even when the tray icon is hidden by
            // Win11's notification area quirks — newly-added tray icons sometimes
            // get swallowed into the overflow popup and aren't immediately visible).
            if let Err(e) = apply_hotkey(app.handle(), &settings.hotkey, run_capture_pipeline) {
                eprintln!("failed to register capture hotkey '{}': {e}", settings.hotkey);
            } else {
                println!("capture hotkey: {}", settings.hotkey);
            }
            if let Err(e) = apply_hotkey(app.handle(), &settings.settings_hotkey, |app| async move {
                show_settings_window(&app);
                Ok(())
            }) {
                eprintln!(
                    "failed to register settings hotkey '{}': {e}",
                    settings.settings_hotkey
                );
            } else {
                println!("settings hotkey: {}", settings.settings_hotkey);
            }

            // ── System tray ───────────────────────────────────────────────────
            // On Windows 11 the notification area sometimes swallows icons that
            // are added at runtime. We:
            //  1. use the window icon PNG that Tauri already validated at build time
            //  2. set a non-empty title (required for the icon to surface on
            //     some Win11 builds — pure `icon` + `tooltip` is sometimes hidden)
            //  3. explicitly call `set_visible(true)` after build
            let icon = app
                .default_window_icon()
                .cloned()
                .ok_or_else(|| AppError::Capture("default window icon missing".into()))?;

            let show_item = MenuItemBuilder::with_id("show", "显示结果窗口").build(app)?;
            let settings_item = MenuItemBuilder::with_id("settings", "设置…").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "退出").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&show_item, &settings_item])
                .separator()
                .items(&[&quit_item])
                .build()?;

            let tray = TrayIconBuilder::new()
                .icon(icon)
                .tooltip("rs-sc")
                .title("rs-sc")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_result_window(app),
                    "settings" => show_settings_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, .. } = event {
                        // Left-click on the tray icon: bring forward whatever
                        // window the user had open last, or open settings.
                        show_settings_window(&tray.app_handle());
                    }
                })
                .build(app)?;

            // Force the icon visible. Win11 occasionally puts freshly-added tray
            // icons into the overflow popup; this at least ensures the OS knows
            // we want it shown.
            let _ = tray.set_visible(true);
            println!("tray icon registered: id={:?}", tray.id());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            test_connection,
            set_result_always_on_top,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build tauri app")
        .run(|_handle, _event| {});
}

// ── Tauri commands ───────────────────────────────────────────────────────────

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> AppResult<Settings> {
    Ok(Settings::load(&state.settings_path)?)
}

#[tauri::command]
async fn save_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    new_settings: Settings,
) -> AppResult<()> {
    // 1. Load the on-disk copy so we can diff the two hotkeys.
    let old = Settings::load(&state.settings_path)?;

    // 2. Validate inputs up front — don't write a half-baked file.
    if new_settings.base_url.trim().is_empty() {
        return Err(AppError::Capture("Base URL 不能为空".into()));
    }
    if new_settings.model.trim().is_empty() {
        return Err(AppError::Capture("Model 不能为空".into()));
    }
    if new_settings.hotkey.trim().is_empty() {
        return Err(AppError::Capture("截屏快捷键不能为空".into()));
    }
    if new_settings.settings_hotkey.trim().is_empty() {
        return Err(AppError::Capture("设置快捷键不能为空".into()));
    }
    if new_settings.hotkey.trim().eq_ignore_ascii_case(new_settings.settings_hotkey.trim()) {
        return Err(AppError::Capture(
            "截屏快捷键和设置快捷键不能相同".into(),
        ));
    }

    // 3. Hot-swap the LLM config (no disk round-trip needed for this part).
    state.llm.set_config(new_settings.into_llm_config()).await;

    // 4. Hot-swap the capture hotkey if it changed.
    if new_settings.hotkey != old.hotkey {
        swap_hotkey(
            &app,
            &old.hotkey,
            &new_settings.hotkey,
            run_capture_pipeline,
        )?;
        *state.current_hotkey.write().await = new_settings.hotkey.clone();
    }

    // 5. Hot-swap the settings hotkey if it changed.
    if new_settings.settings_hotkey != old.settings_hotkey {
        swap_hotkey(
            &app,
            &old.settings_hotkey,
            &new_settings.settings_hotkey,
            |app| async move {
                show_settings_window(&app);
                Ok(())
            },
        )?;
        *state.current_settings_hotkey.write().await = new_settings.settings_hotkey.clone();
    }

    // 6. Persist to disk last — by this point every runtime side-effect has
    //    already succeeded, so a save failure can be reported without leaving
    //    runtime state out of sync.
    new_settings.save(&state.settings_path)?;
    Ok(())
}

/// Replace a global hotkey registration: register the new combo first; if that
/// fails we leave the old one intact and surface the error. Only on success do
/// we unregister the old combo.
fn swap_hotkey<F, Fut>(app: &AppHandle, old: &str, new: &str, action: F) -> AppResult<()>
where
    F: Fn(AppHandle) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
{
    if let Err(e) = apply_hotkey(app, new, action) {
        return Err(AppError::Capture(format!(
            "新快捷键 '{new}' 注册失败: {e}（已保留旧快捷键 '{old}'）"
        )));
    }
    let _ = unregister_hotkey(app, old);
    Ok(())
}

#[tauri::command]
async fn test_connection(state: State<'_, AppState>) -> AppResult<String> {
    state.llm.probe().await
}

/// Toggle the result window's always-on-top state. Called from the pin button
/// in `ui/result.html`. The window also gets re-pinned every time the pipeline
/// shows it, so this is mainly for the user's manual override.
#[tauri::command]
fn set_result_always_on_top(app: AppHandle, on_top: bool) -> AppResult<()> {
    let window = app
        .get_webview_window(RESULT_WINDOW_LABEL)
        .ok_or_else(|| AppError::Capture("result window missing from config".into()))?;
    window
        .set_always_on_top(on_top)
        .map_err(|e| AppError::Capture(format!("set_always_on_top failed: {e}")))?;
    Ok(())
}

// ── Hotkey management ────────────────────────────────────────────────────────

/// Register a global hotkey. The handler runs the supplied async action whenever
/// the key combo is pressed. The action gets the AppHandle and returns an
/// `AppResult`; we just log failures (returning errors from a hotkey callback
/// has nowhere meaningful to go).
///
/// `action` is wrapped in an `Arc` so the closure we hand to winit can be
/// cloned cheaply (winit's callback machinery takes the closure by value and
/// we want to be able to call the same action from multiple hotkey
/// registrations, e.g. capture + settings).
fn apply_hotkey<F, Fut>(app: &AppHandle, hotkey: &str, action: F) -> AppResult<()>
where
    F: Fn(AppHandle) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
{
    let hotkey = hotkey.trim();
    if hotkey.is_empty() {
        return Err(AppError::Capture("empty hotkey".into()));
    }
    let action: Arc<F> = Arc::new(action);
    let app_clone = app.clone();
    app.global_shortcut()
        .on_shortcut(hotkey, move |_app, _sc, ev| {
            if ev.state == ShortcutState::Pressed {
                let h = app_clone.clone();
                let action = action.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(err) = action(h).await {
                        eprintln!("hotkey action failed: {err}");
                    }
                });
            }
        })
        .map_err(|e| AppError::Capture(format!("register hotkey: {e}")))?;
    Ok(())
}

fn unregister_hotkey(app: &AppHandle, hotkey: &str) {
    let hotkey = hotkey.trim();
    if hotkey.is_empty() {
        return;
    }
    if let Err(e) = app.global_shortcut().unregister(hotkey) {
        eprintln!("unregister hotkey '{hotkey}' failed: {e}");
    }
}

// ── Window helpers ────────────────────────────────────────────────────────────

fn show_result_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(RESULT_WINDOW_LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn show_settings_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SETTINGS_WINDOW_LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }

    // The window is declared in tauri.conf.json so it always exists; the
    // fallback below is for the (very unlikely) case it was somehow dropped.
    match WebviewWindowBuilder::new(app, SETTINGS_WINDOW_LABEL, WebviewUrl::App("settings.html".into()))
        .title("rs-sc · 设置")
        .inner_size(480.0, 520.0)
        .resizable(false)
        .center()
        .build()
    {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => {
            eprintln!("failed to create settings window: {e}");
        }
    }
}

// ── Pipeline: hotkey → screen capture → native overlay → translate → show ────

async fn run_capture_pipeline(app: AppHandle) -> AppResult<()> {
    let state: State<'_, AppState> = app.state();

    // Build a fresh cancel handle for THIS capture and install it as the
    // active one. If there was an active capture, it gets cancelled first
    // — its winit overlay disappears and its in-flight AI call winds down
    // within ~100ms. We deliberately do NOT block on the previous task:
    // it cleans up on its own schedule, and our new capture proceeds
    // immediately. This is the core "no silent drops" guarantee.
    let new_capture = ActiveCapture::new();
    let cancel_handle = new_capture.cancel.clone();

    {
        let mut slot = state.active_capture.lock().await;
        if let Some(prev) = slot.take() {
            println!("[capture] interrupting previous in-flight capture");
            prev.cancel();
        }
        *slot = Some(new_capture);
    }

    let outcome = pipeline_inner(&app, state.inner(), &cancel_handle).await;

    // If WE were the active capture, clear the slot so a subsequent press
    // doesn't try to cancel a non-existent task. If a newer press already
    // took our slot, that's fine — we just leave it alone.
    {
        let mut slot = state.active_capture.lock().await;
        if let Some(active) = slot.as_ref() {
            if Arc::ptr_eq(&active.cancel, &cancel_handle) {
                *slot = None;
            }
        }
    }

    outcome
}

async fn pipeline_inner(
    app: &tauri::AppHandle,
    state: &AppState,
    cancel: &Arc<AtomicBool>,
) -> AppResult<()> {
    let cancelled = || cancel.load(Ordering::Relaxed);

    // 1. Capture the screen under the cursor.
    if cancelled() {
        return Ok(());
    }
    let monitor = tokio::task::spawn_blocking(capture::find_cursor_monitor)
        .await
        .map_err(|e| AppError::Capture(format!("find monitor: {e}")))??;

    if cancelled() {
        return Ok(());
    }
    let (rgba, img_w, img_h) = tokio::task::spawn_blocking(move || {
        capture::capture_screen_rgba(monitor.screen)
    })
    .await
    .map_err(|e| AppError::Capture(format!("capture task: {e}")))??;

    // Wrap the full-screen RGBA in an Arc. The capture overlay takes a clone
    // for painting; we keep one here for cropping the selected region. This
    // avoids a second full-screen BitBlt (50-100ms on most systems).
    let rgba = Arc::new(rgba);

    // 2. Hand the image off to the native overlay window and wait for a
    //    selection. The overlay closes itself on mouse-up; a fresh hotkey
    //    press closes it via cancel_capture(), which makes the winit overlay
    //    exit, dropping our `event_tx` half and causing recv() to return Err.
    let (event_tx, event_rx) = mpsc::channel::<CaptureEvent>();
    capture_window::start_capture(
        rgba.clone(),
        img_w,
        img_h,
        monitor.monitor.x,
        monitor.monitor.y,
        event_tx,
    );

    let selection = match event_rx.recv() {
        Ok(s) => s,
        Err(_) => {
            // Channel closed — either user cancelled (Esc / right-click /
            // close button) or a new hotkey press fired cancel_capture().
            // In both cases we just exit cleanly; no output.
            println!("[capture] aborted before selection completed");
            return Ok(());
        }
    };

    if cancelled() {
        return Ok(());
    }

    let rect = match selection {
        CaptureEvent::Selection { x, y, w, h } => CaptureRect { x, y, width: w, height: h },
        CaptureEvent::Cancelled => {
            println!("[capture] user cancelled selection");
            return Ok(());
        }
    };

    // 3. Crop directly from the in-memory screenshot. No second capture.
    let cropped = capture::crop_rgba(&rgba, img_w, rect.x, rect.y, rect.width, rect.height);
    let png_bytes = capture::encode_png(&cropped, rect.width, rect.height)?;
    let png_b64 = BASE64.encode(&png_bytes);

    // 4. Bring up the result window IMMEDIATELY with a "translating…" state.
    //    The user gets visual feedback within milliseconds of releasing the
    //    mouse, instead of staring at their desktop while we wait for the
    //    network round-trip.
    if let Err(e) = show_result_window_busy(app, &png_b64) {
        eprintln!("failed to show result window: {e}");
    }

    if cancelled() {
        // User pressed the hotkey again while we were preparing. Drop the
        // pending result — the new capture will produce its own.
        let _ = emit_result_error(app, "已取消");
        return Ok(());
    }

    // 5. Now make the actual AI call. The result window is already visible
    //    and showing the cropped image, so even a slow API doesn't feel
    //    like a hang. translate_png polls the cancel flag internally so a
    //    fresh hotkey press can abort the in-flight HTTP within ~100ms.
    let translated = match state.llm.translate_png(&png_bytes, Some(cancel)).await {
        Ok(t) => t,
        Err(e) => {
            if matches!(e, AppError::Capture(ref m) if m == "cancelled") {
                println!("[capture] AI call cancelled by new hotkey press");
                let _ = emit_result_error(app, "已取消");
                return Ok(());
            }
            let _ = emit_result_error(app, &e.to_string());
            return Err(e);
        }
    };

    // 6. Push the final text into the already-visible window. If we got
    //    cancelled in the gap between AI returning and now, suppress the
    //    output — a newer capture may already be showing its own result.
    if cancelled() {
        return Ok(());
    }
    emit_result_loaded(app, &translated, &png_b64)?;
    Ok(())
}

/// Show the result window with the cropped image already filled in, but the
/// text area showing "正在翻译…". Used right after capture selection so the
/// user has immediate visual feedback that the work is in progress.
fn show_result_window_busy(app: &tauri::AppHandle, image_b64: &str) -> AppResult<()> {
    let window = app
        .get_webview_window(RESULT_WINDOW_LABEL)
        .ok_or_else(|| AppError::Capture("result window missing from config".into()))?;

    position_result_window_near_cursor(app, &window);

    window.show()?;
    window.set_focus()?;

    window.emit(
        RESULT_BUSY_EVENT,
        serde_json::json!({ "imageBase64": image_b64 }),
    )?;
    Ok(())
}

fn emit_result_loaded(
    app: &tauri::AppHandle,
    text: &str,
    image_b64: &str,
) -> AppResult<()> {
    let window = app
        .get_webview_window(RESULT_WINDOW_LABEL)
        .ok_or_else(|| AppError::Capture("result window missing from config".into()))?;
    window.emit(
        RESULT_LOADED_EVENT,
        serde_json::json!({
            "text": text,
            "imageBase64": image_b64,
        }),
    )?;
    Ok(())
}

fn emit_result_error(app: &tauri::AppHandle, message: &str) -> AppResult<()> {
    let window = app
        .get_webview_window(RESULT_WINDOW_LABEL)
        .ok_or_else(|| AppError::Capture("result window missing from config".into()))?;
    window.emit(RESULT_ERROR_EVENT, serde_json::json!({ "error": message }))?;
    Ok(())
}

fn position_result_window_near_cursor(_app: &tauri::AppHandle, window: &tauri::WebviewWindow) {
    let (mx, my) = cursor_position_safe();
    let (sx, sy) = primary_screen_size_safe();
    let win_w = 560.0_f64;
    let win_h = 360.0_f64;
    let px = if sx > 0.0 { (mx + 24.0).min(sx - win_w - 24.0).max(24.0) } else { 120.0 };
    let py = if sy > 0.0 { (my + 24.0).min(sy - win_h - 24.0).max(24.0) } else { 120.0 };
    let _ = window.set_position(PhysicalPosition::new(px, py));
    let _ = window.set_size(PhysicalSize::new(win_w, win_h));
    // Re-assert always-on-top on every show. Tauri's `alwaysOnTop: true` in
    // tauri.conf.json sets the initial flag, but if the user uses a system
    // shortcut (Win+Tab, "Show windows stacked") or our own future toggle,
    // it can drift. Calling this every time keeps the result window pinned
    // above the user's regular work no matter what.
    let _ = window.set_always_on_top(true);
}

#[cfg(target_os = "windows")]
fn cursor_position_safe() -> (f64, f64) {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
    }
    let mut pt = Point { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x as f64, pt.y as f64)
}
#[cfg(not(target_os = "windows"))]
fn cursor_position_safe() -> (f64, f64) {
    (200.0, 200.0)
}

#[cfg(target_os = "windows")]
fn primary_screen_size_safe() -> (f64, f64) {
    use screenshots::Screen;
    if let Ok(screens) = Screen::all() {
        if let Some(primary) = screens.into_iter().find(|s| s.display_info.is_primary) {
            return (
                primary.display_info.width as f64,
                primary.display_info.height as f64,
            );
        }
    }
    (1920.0, 1080.0)
}
#[cfg(not(target_os = "windows"))]
fn primary_screen_size_safe() -> (f64, f64) {
    (1920.0, 1080.0)
}

// Make sure the LlmConfig type stays reachable from this module — used by
// settings::Settings::into_llm_config to construct the runtime config.
#[allow(dead_code)]
type _LlmConfig = LlmConfig;
