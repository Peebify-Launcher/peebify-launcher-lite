// ------------ Window Manager ------------
// Looks after the main window and the tray icon: showing, hiding and minimizing, remembering size and position,
// and deciding when it is safe to quit.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use super::BackendState;
use super::{game_profiles, ok_response, ok_with};

static BACKGROUNDED: AtomicBool = AtomicBool::new(false);
static WEBVIEW_HIDDEN: AtomicBool = AtomicBool::new(false);

pub fn is_backgrounded() -> bool {
    BACKGROUNDED.load(Ordering::SeqCst)
}

const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_ID: &str = "peebify-tray";

pub(crate) const WEBVIEW_BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion,IntensiveWakeUpThrottling \
     --disable-backgrounding-occluded-windows --disable-background-timer-throttling \
     --disable-renderer-backgrounding";

const FIRST_PAINT_NUDGE_HOLD: std::time::Duration = std::time::Duration::from_millis(32);
const FIRST_PAINT_SETTLE: std::time::Duration = std::time::Duration::from_millis(48);
const RECORDING_FINALIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const SCALE_ANIMATION: std::time::Duration = std::time::Duration::from_millis(240);
const SCALE_PIN: std::time::Duration = std::time::Duration::from_millis(40);
const SCALE_SETTLE: std::time::Duration = std::time::Duration::from_millis(60);
const SCALE_TAIL: std::time::Duration = std::time::Duration::from_millis(40);
const SCALE_FADE: std::time::Duration = std::time::Duration::from_millis(110);
const QUIT_WHEN_IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(2);
const KEEP_IN_TRAY_LABEL: &str = "Keep running in tray";
const QUIT_ANYWAY_LABEL: &str = "Quit anyway";
const CANCEL_QUIT_LABEL: &str = "Cancel";
const DRAG_STRIP_LOGICAL: f64 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadySource {
    Renderer,
    Backstop,
}

impl ReadySource {
    fn label(self) -> &'static str {
        match self {
            ReadySource::Renderer => "the renderer",
            ReadySource::Backstop => "the +2s backstop",
        }
    }
}

pub struct WindowManager {
    app: AppHandle,
    tray_active: AtomicBool,
    is_quitting: AtomicBool,
    save_generation: AtomicU64,
    zoom_generation: AtomicU64,
    zoom_animations: AtomicUsize,
    zoom_gate: tauri::async_runtime::Mutex<()>,
    ui_zoom: AtomicU64,
    ready_shown: AtomicBool,
    was_minimized: AtomicBool,
    was_maximized: AtomicBool,
    renderer_signaled: AtomicBool,
    exit_after_game: AtomicBool,
    quit_when_idle: AtomicBool,
    quit_prompt_open: AtomicBool,
}

fn main_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(MAIN_WINDOW_LABEL)
}

impl WindowManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            tray_active: AtomicBool::new(false),
            is_quitting: AtomicBool::new(false),
            save_generation: AtomicU64::new(0),
            zoom_generation: AtomicU64::new(0),
            zoom_animations: AtomicUsize::new(0),
            zoom_gate: tauri::async_runtime::Mutex::new(()),
            ui_zoom: AtomicU64::new(1.0f64.to_bits()),
            ready_shown: AtomicBool::new(false),
            was_minimized: AtomicBool::new(false),
            was_maximized: AtomicBool::new(false),
            renderer_signaled: AtomicBool::new(false),
            exit_after_game: AtomicBool::new(false),
            quit_when_idle: AtomicBool::new(false),
            quit_prompt_open: AtomicBool::new(false),
        })
    }

    pub fn renderer_signaled(&self) -> bool {
        self.renderer_signaled.load(Ordering::SeqCst)
    }

    pub fn tray_active(&self) -> bool {
        self.tray_active.load(Ordering::SeqCst)
    }

    fn state(&self) -> tauri::State<'_, BackendState> {
        self.app.state::<BackendState>()
    }

    fn behavior(&self, key: &str) -> Value {
        self.state().config.get(&format!("behavior.{key}"))
    }

    pub fn create_tray(self: &Arc<Self>) {
        if self.tray_active.swap(true, Ordering::SeqCst) {
            return;
        }
        let result = (|| -> Result<(), String> {
            let icon_path = self
                .app
                .path()
                .resource_dir()
                .map(|d| d.join("icons").join("app.png"))
                .map_err(|e| e.to_string())?;
            let icon = tauri::image::Image::from_path(&icon_path)
                .map_err(|e| format!("tray icon load failed: {e}"))?;

            let menu = self.build_tray_menu()?;
            let me = Arc::clone(self);
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(icon)
                .tooltip("Peebify Launcher")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(move |_tray, event| match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    }
                    | TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } => me.show_window(),
                    _ => {}
                })
                .build(&self.app)
                .map_err(|e| e.to_string())?;
            Ok(())
        })();

        match result {
            Ok(()) => log::info!("System tray created"),
            Err(e) => {
                self.tray_active.store(false, Ordering::SeqCst);
                log::error!("Failed to create system tray: {e}");
            }
        }
    }

    fn build_tray_menu(&self) -> Result<tauri::menu::Menu<tauri::Wry>, String> {
        let state = self.state();
        let launchable = |id: &str| {
            !state.game.is_game_active_id(id)
                && state.engine.queue.launch_blocker(id).is_none()
                && !state.game.update_left_unfinished(id)
        };
        let app = &self.app;

        let show = MenuItemBuilder::with_id("tray-show", "Show Launcher")
            .build(app)
            .map_err(|e| e.to_string())?;
        let quit = MenuItemBuilder::with_id("tray-quit", "Quit")
            .build(app)
            .map_err(|e| e.to_string())?;
        let sep1 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
        let sep2 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;

        let library: Vec<String> = match state.config.get("library.visible") {
            Value::Array(ids) => ids
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|id| game_profiles::GAME_IDS.contains(id))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let library = if library.is_empty() {
            game_profiles::GAME_IDS
                .iter()
                .map(|id| (*id).to_string())
                .collect()
        } else {
            library
        };

        let mut quick_items = Vec::with_capacity(library.len());
        for id in &library {
            let profile = game_profiles::profile(id);
            let installed = state
                .config
                .get(&format!("games.{id}.gamePath"))
                .as_str()
                .is_some_and(|p| !p.is_empty());
            let item = MenuItemBuilder::with_id(
                format!("tray-launch-{id}"),
                game_profiles::display_name(profile),
            )
            .enabled(installed && launchable(id))
            .build(app)
            .map_err(|e| e.to_string())?;
            quick_items.push(item);
        }
        let mut quick = SubmenuBuilder::new(app, "Launch a game");
        for item in &quick_items {
            quick = quick.item(item);
        }
        let quick = quick.build().map_err(|e| e.to_string())?;

        MenuBuilder::new(app)
            .item(&show)
            .item(&sep1)
            .item(&quick)
            .item(&sep2)
            .item(&quit)
            .build()
            .map_err(|e| e.to_string())
    }

    fn report_tray_launch(&self, target: Option<&str>, result: &Value) {
        if result["success"] == Value::Bool(true) {
            return;
        }
        let state = self.state();
        let game_id = target
            .map(str::to_string)
            .unwrap_or_else(|| state.config.active_game_id());
        let profile = game_profiles::profile(&game_id);
        let error = result["error"]
            .as_str()
            .unwrap_or("The game could not be launched.");
        if state.game.is_game_active_id(game_profiles::profile_id(profile)) {
            log::info!("Tray launch skipped: {error}");
            return;
        }
        log::warn!("Tray launch failed: {error}");
        let _ = self
            .app
            .emit(
                "game-launch-failed",
                json!({ "gameId": game_profiles::profile_id(profile), "reason": error }),
            );
        super::platform::notify_if_backgrounded(
            &self.app,
            &format!("{} did not start", game_profiles::display_name(profile)),
            error,
        );
    }

    pub fn update_tray_menu(self: &Arc<Self>) {
        if !self.tray_active.load(Ordering::SeqCst) {
            return;
        }
        let Some(tray) = self.app.tray_by_id(TRAY_ID) else {
            return;
        };
        match self.build_tray_menu() {
            Ok(menu) => {
                let _ = tray.set_menu(Some(menu));
            }
            Err(e) => log::warn!("Failed to rebuild tray menu: {e}"),
        }
    }

    pub fn destroy_tray(&self) {
        if self.tray_active.swap(false, Ordering::SeqCst) {
            let _ = self.app.remove_tray_by_id(TRAY_ID);
            log::info!("System tray destroyed");
        }
    }

    pub fn wire_menu_handler(self: &Arc<Self>) {
        let me = Arc::clone(self);
        self.app.clone().on_menu_event(move |_app, event| {
            let me = Arc::clone(&me);
            match event.id().0.as_str() {
                "tray-show" => me.show_window(),
                "tray-quit" => me.request_quit(),
                id if id.starts_with("tray-launch-") => {
                    let game_id = id.trim_start_matches("tray-launch-").to_string();
                    tauri::async_runtime::spawn(async move {
                        let game = me.state().game.clone();
                        let result = game.launch_game(Some(&game_id)).await;
                        me.report_tray_launch(Some(&game_id), &result);
                    });
                }
                _ => {}
            }
        });
    }

    pub fn show_window(self: &Arc<Self>) {
        self.show_main_window(true);
    }

    fn show_main_window(self: &Arc<Self>, announce_restore: bool) {
        self.exit_after_game.store(false, Ordering::SeqCst);
        self.quit_when_idle.store(false, Ordering::SeqCst);
        let Some(win) = main_window(&self.app) else {
            return;
        };
        if win.is_minimized().unwrap_or(false) {
            self.was_minimized.store(false, Ordering::SeqCst);
            let _ = win.unminimize();
        }
        let _ = win.show();
        bring_on_screen(&win);
        let _ = win.set_focus();
        set_memory_low(&win, false);

        self.destroy_tray();
        self.notify_power_save(false);
        if announce_restore {
            let _ = self.app.emit("window-restored", Value::Null);
        }
    }

    pub fn hide_window(self: &Arc<Self>) {
        let Some(win) = main_window(&self.app) else {
            return;
        };
        self.create_tray();
        if self.tray_active() {
            let _ = win.hide();
        } else {
            log::warn!("The tray icon is unavailable, so the launcher is minimized instead of hidden.");
            let _ = win.minimize();
        }
        set_memory_low(&win, true);
        self.notify_power_save(true);
    }

    fn show_minimized(&self, win: &WebviewWindow) {
        let _ = win.minimize();
        let _ = win.show();
        set_memory_low(win, true);
        self.notify_power_save(true);
    }

    pub fn finish_recording_before_exit(&self) {
        let overlay = self.state().overlay.clone();
        if overlay.active_recording_path().is_none() {
            return;
        }
        log::info!("Saving the active recording before exit.");
        if !overlay.stop_recording_and_wait(RECORDING_FINALIZE_TIMEOUT) {
            log::warn!(
                "The active recording did not finish saving within {}s of exit.",
                RECORDING_FINALIZE_TIMEOUT.as_secs()
            );
        }
    }

    pub fn minimize_window(self: &Arc<Self>) {
        let Some(win) = main_window(&self.app) else {
            return;
        };
        match self
            .behavior("minimizeAction")
            .as_str()
            .unwrap_or("minimize")
        {
            "tray" => self.hide_window(),
            "close" => self.request_quit(),
            _ => {
                let _ = win.minimize();
                set_memory_low(&win, true);
                self.notify_power_save(true);
            }
        }
    }

    pub fn quit_app(&self) {
        self.is_quitting.store(true, Ordering::SeqCst);
        self.finish_recording_before_exit();
        super::game_file_ops::log_unfinished_work(&self.app);
        self.save_open_sessions();
        self.state().config.flush();
        super::logger::flush();
        self.app.exit(0);
    }

    pub fn is_quitting(&self) -> bool {
        self.is_quitting.load(Ordering::SeqCst)
    }

    fn save_open_sessions(&self) {
        let game = self.state().game.clone();
        if game.is_game_running() {
            game.playtime.checkpoint();
        } else {
            game.playtime.flush_all(&self.app);
        }
    }

    pub fn request_quit(self: &Arc<Self>) {
        if self.is_quitting() {
            return;
        }
        if session_ending() {
            self.quit_app();
            return;
        }
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            match unfinished_risky_work(&me.app).await {
                Some(work) => me.confirm_quit(work),
                None => me.quit_app(),
            }
        });
    }

    fn confirm_quit(self: &Arc<Self>, work: RiskyWork) {
        if self.quit_prompt_open.swap(true, Ordering::SeqCst) {
            if let Some(win) = main_window(&self.app) {
                let _ = win.set_focus();
            }
            return;
        }
        log::info!(
            "Quit requested during a {} ({}). Asking first.",
            work.op.label(),
            work.game_id
        );
        use tauri_plugin_dialog::{
            DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
        };
        let name = game_profiles::display_name(game_profiles::profile(&work.game_id));
        let mut dialog = self
            .app
            .dialog()
            .message(work.op.warning(name))
            .title("Quit Peebify?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::YesNoCancelCustom(
                KEEP_IN_TRAY_LABEL.to_string(),
                QUIT_ANYWAY_LABEL.to_string(),
                CANCEL_QUIT_LABEL.to_string(),
            ));
        if let Some(win) = main_window(&self.app).filter(|w| w.is_visible().unwrap_or(false)) {
            dialog = dialog.parent(&win);
        }
        let me = Arc::clone(self);
        dialog.show_with_result(move |result| {
            me.quit_prompt_open.store(false, Ordering::SeqCst);
            match result {
                MessageDialogResult::Custom(choice) if choice == QUIT_ANYWAY_LABEL => {
                    log::info!("Quit confirmed during a {}.", work.op.label());
                    me.quit_app();
                }
                MessageDialogResult::Custom(choice) if choice == KEEP_IN_TRAY_LABEL => {
                    me.hide_window();
                }
                _ => log::info!("Quit cancelled."),
            }
        });
    }

    fn has_unfinished_work(&self) -> bool {
        let state = self.state();
        state.engine.queue.is_busy()
            || game_profiles::GAME_IDS
                .iter()
                .any(|id| super::game_file_ops::is_uninstalling(id))
    }

    fn quit_when_work_finishes(self: &Arc<Self>) {
        log::info!("Other work is still running, so the launcher waits in the tray and exits when it finishes.");
        self.create_tray();
        if self.quit_when_idle.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(QUIT_WHEN_IDLE_POLL).await;
                if !me.quit_when_idle.load(Ordering::SeqCst) {
                    return;
                }
                if !me.has_unfinished_work() && !me.state().game.is_game_running() {
                    break;
                }
            }
            if me.quit_when_idle.swap(false, Ordering::SeqCst) {
                log::info!("The remaining work finished. Exiting now.");
                me.quit_app();
            }
        });
    }

    fn notify_power_save(&self, suspended: bool) {
        BACKGROUNDED.store(suspended, Ordering::SeqCst);
        let _ = self.app.emit("renderer-power-save", suspended);
    }

    pub fn perform_launch_action(self: &Arc<Self>) {
        let action = self.behavior("launchAction");
        let action = action.as_str().unwrap_or("minimize");
        log::info!("Performing post-launch action: \"{action}\"");
        match action {
            "tray" => self.hide_window(),
            "close" => self.close_until_game_exits(),
            "minimize" => {
                if let Some(win) = main_window(&self.app) {
                    let _ = win.minimize();
                    set_memory_low(&win, true);
                    self.notify_power_save(true);
                }
            }
            _ => {}
        }
    }

    fn close_until_game_exits(self: &Arc<Self>) {
        if !self.state().game.is_game_running() {
            if self.has_unfinished_work() {
                self.hide_window();
                self.quit_when_work_finishes();
            } else {
                self.quit_app();
            }
            return;
        }
        self.exit_after_game.store(true, Ordering::SeqCst);
        self.destroy_tray();
        if let Some(win) = main_window(&self.app) {
            let _ = win.hide();
            set_memory_low(&win, true);
        }
        self.notify_power_save(true);
        log::info!("Launcher closed for the game. It keeps tracking playtime and exits when the game does.");
    }

    pub fn exit_if_closed_for_game(self: &Arc<Self>) {
        if !self.exit_after_game.swap(false, Ordering::SeqCst) {
            return;
        }
        log::info!("Game closed after the launcher was closed for it. Exiting now.");
        if self.has_unfinished_work() {
            self.quit_when_work_finishes();
        } else {
            self.quit_app();
        }
    }

    pub fn reopen_after_game(self: &Arc<Self>) {
        if self.exit_after_game.load(Ordering::SeqCst) {
            return;
        }
        if self.behavior("reopenAfterGameClose") == Value::Bool(false) {
            return;
        }
        log::info!("Reopening launcher after game close");
        self.show_window();
    }

    pub fn handle_close_requested(self: &Arc<Self>) -> bool {
        if self.is_quitting.load(Ordering::SeqCst) {
            return false;
        }
        match self.behavior("closeAction").as_str().unwrap_or("close") {
            "minimize" => {
                if let Some(win) = main_window(&self.app) {
                    let _ = win.minimize();
                    set_memory_low(&win, true);
                    self.notify_power_save(true);
                }
                true
            }
            "tray" => {
                self.hide_window();
                true
            }
            _ => {
                self.request_quit();
                true
            }
        }
    }

    pub fn on_main_window_destroyed(&self) {
        if self.is_quitting() {
            return;
        }
        log::warn!("The main window was destroyed outside a quit. Exiting.");
        self.is_quitting.store(true, Ordering::SeqCst);
        self.app.exit(0);
    }

    pub fn on_window_resized(self: &Arc<Self>) {
        let Some(win) = main_window(&self.app) else {
            return;
        };
        let minimized = win.is_minimized().unwrap_or(false);
        let was = self.was_minimized.swap(minimized, Ordering::SeqCst);
        if minimized && !was {
            set_memory_low(&win, true);
            self.notify_power_save(true);
        } else if !minimized && was {
            set_memory_low(&win, false);
            self.notify_power_save(false);
            let _ = self.app.emit("window-restored", Value::Null);
        } else if !minimized
            && WEBVIEW_HIDDEN.load(Ordering::SeqCst)
            && win.is_visible().unwrap_or(false)
        {
            set_memory_low(&win, false);
            self.notify_power_save(false);
        }
        let maximized = win.is_maximized().unwrap_or(false);
        if self.was_maximized.swap(maximized, Ordering::SeqCst) != maximized {
            let _ = win.emit("window:maximized-changed", json!({ "maximized": maximized }));
        }
    }

    pub fn schedule_window_state_save(self: &Arc<Self>) {
        if self.behavior("rememberWindowState") == Value::Bool(false) {
            return;
        }
        let generation = self.save_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if me.save_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            me.save_window_state().await;
        });
    }

    async fn save_window_state(self: &Arc<Self>) {
        let Some(win) = main_window(&self.app) else {
            return;
        };
        if win.is_minimized().unwrap_or(false) || !win.is_visible().unwrap_or(false) {
            return;
        }
        let is_maximized = win.is_maximized().unwrap_or(false);
        let scale = win.scale_factor().unwrap_or(1.0);
        let (Ok(size), Ok(position)) = (win.outer_size(), win.outer_position()) else {
            return;
        };
        let zoom = self.ui_zoom();
        let design_w = f64::from(size.width) / scale / zoom;
        let design_h = f64::from(size.height) / scale / zoom;
        if !is_maximized && (design_w <= 0.0 || design_h <= 0.0) {
            return;
        }

        let current = self.state().config.get("window");
        let pick = |key: &str, live: f64| -> Value {
            if is_maximized {
                current.get(key).cloned().unwrap_or(Value::Null)
            } else {
                json!(live.round())
            }
        };
        let new_config = json!({
            "width": pick("width", design_w),
            "height": pick("height", design_h),
            "x": pick("x", f64::from(position.x) / scale),
            "y": pick("y", f64::from(position.y) / scale),
            "maximized": is_maximized,
        });
        super::config::set_config_value(&self.app, "window", new_config);
    }

    pub fn restore_window_state(&self) {
        let Some(win) = main_window(&self.app) else {
            return;
        };
        let window_config = if self.behavior("rememberWindowState") == Value::Bool(false) {
            Value::Null
        } else {
            self.state().config.get("window")
        };

        let get = |key: &str| window_config.get(key).and_then(Value::as_f64);
        if window_config.is_object() {
            log::info!("display: restoring saved window state {window_config}");
        }
        let (design_w, design_h) = DESIGN_SIZE_LOGICAL;
        let (w, h) = match (get("width"), get("height")) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
            _ => (design_w, design_h),
        };
        let saved_at = match (get("x"), get("y")) {
            (Some(x), Some(y)) => Some(Bounds { x, y, w, h }),
            _ => None,
        };
        let zoom = zoom_for_landing_monitor(&win, saved_at, self.user_ui_scale());
        self.set_ui_zoom(&win, zoom);
        let (w, h) = (w * zoom, h * zoom);
        match saved_at.and_then(|at| place_on_monitors(&win, Bounds { w, h, ..at })) {
            Some((bounds, centered)) => {
                if centered {
                    log::info!(
                        "display: saved window bounds are off every monitor, centering at {},{}",
                        bounds.x,
                        bounds.y
                    );
                }
                let _ = win.set_position(tauri::PhysicalPosition::new(
                    bounds.x.round() as i32,
                    bounds.y.round() as i32,
                ));
                let _ = win.set_size(tauri::PhysicalSize::new(
                    bounds.w.round() as u32,
                    bounds.h.round() as u32,
                ));
            }
            None => {
                let _ = win.set_size(tauri::LogicalSize::new(w, h));
                let _ = win.center();
            }
        }
        fit_to_work_area(&win, zoom);
        if window_config.get("maximized") == Some(&Value::Bool(true)) {
            let _ = win.maximize();
        }
    }

    fn ui_zoom(&self) -> f64 {
        f64::from_bits(self.ui_zoom.load(Ordering::SeqCst))
    }

    fn user_ui_scale(&self) -> f64 {
        parse_ui_scale(&self.behavior("uiScale"))
    }

    pub fn apply_ui_scale(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let _turn = me.zoom_gate.lock().await;
            me.animate_rezoom().await;
        });
    }

    fn window_and_zoom(&self) -> Option<(WebviewWindow, f64)> {
        let win = main_window(&self.app)?;
        if win.is_minimized().unwrap_or(false) || win.is_fullscreen().unwrap_or(false) {
            return None;
        }
        let monitor = win.current_monitor().ok()??;
        let (area, scale) = work_area_of(&monitor);
        let zoom = interface_zoom(area, scale, self.user_ui_scale());
        Some((win, zoom))
    }

    async fn animate_rezoom(&self) {
        let Some((win, to)) = self.window_and_zoom() else {
            return;
        };
        let from = self.ui_zoom();
        if to == from {
            return;
        }

        struct Running<'a>(&'a AtomicUsize);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.zoom_animations.fetch_add(1, Ordering::SeqCst);
        let _running = Running(&self.zoom_animations);

        if win.is_maximized().unwrap_or(false) {
            self.emit_scale_phase("fade-out", 1.0);
            tokio::time::sleep(SCALE_FADE).await;
            self.set_ui_zoom(&win, to);
            tokio::time::sleep(SCALE_SETTLE).await;
            self.emit_scale_phase("fade-in", 1.0);
            return;
        }

        let (Ok(position), Ok(size)) = (win.outer_position(), win.outer_size()) else {
            self.set_ui_zoom(&win, to);
            return;
        };
        let start = Bounds {
            x: f64::from(position.x),
            y: f64::from(position.y),
            w: f64::from(size.width),
            h: f64::from(size.height),
        };
        let ratio = to / from;
        let end = Bounds { w: start.w * ratio, h: start.h * ratio, ..start };
        let stage = if ratio > 1.0 { end } else { start };

        let (min_w, min_h) = MIN_WINDOW_LOGICAL;
        let lower = from.min(to);
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(min_w * lower, min_h * lower)));

        self.emit_scale_phase("pin", ratio);
        tokio::time::sleep(SCALE_PIN).await;
        if stage != start {
            set_window_bounds(&win, stage);
        }
        tokio::time::sleep(SCALE_SETTLE).await;
        self.emit_scale_phase("start", ratio);
        tokio::time::sleep(SCALE_ANIMATION + SCALE_TAIL).await;

        apply_scale_swap(&win, to, end);
        let (min_w, min_h) = MIN_WINDOW_LOGICAL;
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(min_w * to, min_h * to)));
        self.ui_zoom.store(to.to_bits(), Ordering::SeqCst);
        log::info!("display: interface zoom {from} -> {to}");
        self.emit_scale_phase("end", ratio);
        fit_to_work_area(&win, to);
    }

    fn emit_scale_phase(&self, phase: &str, ratio: f64) {
        let _ = self.app.emit_to(
            MAIN_WINDOW_LABEL,
            "ui-scale-transition",
            json!({ "phase": phase, "ratio": ratio, "ms": SCALE_ANIMATION.as_millis() as u64 }),
        );
    }

    fn set_ui_zoom(&self, win: &WebviewWindow, zoom: f64) {
        let (min_w, min_h) = MIN_WINDOW_LOGICAL;
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(min_w * zoom, min_h * zoom)));
        if let Err(e) = win.set_zoom(zoom) {
            log::warn!("display: could not zoom the interface to {zoom}: {e}");
            return;
        }
        let previous = self.ui_zoom.swap(zoom.to_bits(), Ordering::SeqCst);
        if f64::from_bits(previous) != zoom {
            log::info!("display: interface zoom {} -> {zoom}", f64::from_bits(previous));
        }
    }

    pub fn schedule_zoom_check(self: &Arc<Self>) {
        if self.zoom_animations.load(Ordering::SeqCst) > 0 {
            return;
        }
        let generation = self.zoom_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            if me.zoom_generation.load(Ordering::SeqCst) == generation {
                me.rezoom_for_current_monitor();
            }
        });
    }

    fn rezoom_for_current_monitor(&self) {
        let Some((win, zoom)) = self.window_and_zoom() else {
            return;
        };
        let previous = self.ui_zoom();
        if zoom == previous {
            return;
        }
        self.set_ui_zoom(&win, zoom);
        if win.is_maximized().unwrap_or(false) {
            return;
        }
        let (Ok(position), Ok(size)) = (win.outer_position(), win.outer_size()) else {
            return;
        };
        let ratio = zoom / previous;
        let (w, h) = (f64::from(size.width) * ratio, f64::from(size.height) * ratio);
        let x = f64::from(position.x) + (f64::from(size.width) - w) / 2.0;
        let y = f64::from(position.y) + (f64::from(size.height) - h) / 2.0;
        let _ = win.set_size(tauri::PhysicalSize::new(w.round() as u32, h.round() as u32));
        let _ = win.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32));
        fit_to_work_area(&win, zoom);
    }

    pub fn log_display_environment(&self) {
        let Some(win) = main_window(&self.app) else {
            return;
        };
        match win.available_monitors() {
            Ok(monitors) => {
                for (index, monitor) in monitors.iter().enumerate() {
                    let position = monitor.position();
                    let size = monitor.size();
                    let work = monitor.work_area();
                    log::info!(
                        "display: monitor {index} {} at {},{} size {}x{} scale {} work area {},{} {}x{}",
                        monitor.name().map(String::as_str).unwrap_or("unnamed"),
                        position.x,
                        position.y,
                        size.width,
                        size.height,
                        monitor.scale_factor(),
                        work.position.x,
                        work.position.y,
                        work.size.width,
                        work.size.height
                    );
                }
            }
            Err(e) => log::warn!("display: could not list the monitors: {e}"),
        }
        match (win.outer_position(), win.outer_size(), win.scale_factor()) {
            (Ok(position), Ok(size), Ok(scale)) => log::info!(
                "display: main window at {},{} size {}x{} scale {scale} zoom {} maximized {}",
                position.x,
                position.y,
                size.width,
                size.height,
                self.ui_zoom(),
                win.is_maximized().unwrap_or(false)
            ),
            _ => log::warn!("display: could not read the main window bounds"),
        }
    }

    async fn prime_first_paint(app: &AppHandle) {
        let Some(win) = main_window(app) else {
            return;
        };
        if win.is_visible().unwrap_or(false)
            || win.is_maximized().unwrap_or(false)
            || win.is_fullscreen().unwrap_or(false)
        {
            return;
        }
        let Ok(size) = win.inner_size() else {
            return;
        };
        if size.width == 0 || size.height == 0 {
            return;
        }
        let widened = tauri::PhysicalSize::new(size.width + 1, size.height);
        if win.set_size(widened).is_err() {
            return;
        }
        tokio::time::sleep(FIRST_PAINT_NUDGE_HOLD).await;
        let _ = win.set_size(size);
        tokio::time::sleep(FIRST_PAINT_SETTLE).await;
        log::debug!("ready-to-show: first-paint primed while hidden.");
    }

    fn reveal_main_window(self: &Arc<Self>, full: bool) {
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            Self::prime_first_paint(&me.app).await;
            if full {
                me.show_main_window(false);
            } else if let Some(win) = main_window(&me.app) {
                let _ = win.show();
                let _ = win.set_focus();
                if WEBVIEW_HIDDEN.load(Ordering::SeqCst) {
                    set_memory_low(&win, false);
                    me.notify_power_save(false);
                }
            }
            log::info!(
                "ready-to-show: window.show() issued at {} ms.",
                super::logger::since_boot_ms()
            );
            let _ = me.app.emit("main-window-revealed", ());
        });
    }

    pub fn mark_renderer_signaled(&self) -> bool {
        self.renderer_signaled.swap(true, Ordering::SeqCst)
    }

    pub fn ready_to_show(self: &Arc<Self>, source: ReadySource) {
        if !self.ready_shown.swap(true, Ordering::SeqCst) {
            log::info!("ready-to-show from {}.", source.label());
            let is_boot_launch = super::platform::is_boot_launch();
            let start_on_boot = self.behavior("startOnBoot") == Value::Bool(true);
            if is_boot_launch && start_on_boot {
                let action = self.behavior("startOnBootAction");
                let action = action.as_str().unwrap_or("open");
                log::info!("ready-to-show: boot launch, action={action}");
                match action {
                    "minimized" => {
                        if let Some(win) = main_window(&self.app) {
                            self.show_minimized(&win);
                        }
                    }
                    "tray" => self.hide_window(),
                    _ => self.reveal_main_window(true),
                }
            } else {
                self.reveal_main_window(false);
            }
        } else {
            log::debug!(
                "ready-to-show from {}: already shown, leaving window visibility unchanged.",
                source.label()
            );
        }
    }
}

// ------------ Quit Confirmation ------------
// Some jobs are risky to cut off (moves, uninstalls, unpacking, patching, repairs). Quitting while one runs asks first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RiskyOp {
    Move,
    Uninstall,
    Unpack,
    Patch,
    Repair,
}

impl RiskyOp {
    fn label(self) -> &'static str {
        match self {
            RiskyOp::Move => "move",
            RiskyOp::Uninstall => "uninstall",
            RiskyOp::Unpack => "unpack",
            RiskyOp::Patch => "update",
            RiskyOp::Repair => "repair",
        }
    }

    fn warning(self, name: &str) -> String {
        match self {
            RiskyOp::Move => format!(
                "{name} is still being moved. If you quit now, the move is undone and has to start over."
            ),
            RiskyOp::Uninstall => format!(
                "{name} is still being uninstalled. If you quit now, its folder is left partly removed."
            ),
            RiskyOp::Unpack => format!(
                "{name} is still being unpacked. If you quit now, the unpack has to start over."
            ),
            RiskyOp::Patch => format!(
                "{name} is still being updated. If you quit now, it can't be played until the update finishes."
            ),
            RiskyOp::Repair => format!(
                "{name} is still being repaired. If you quit now, it can't be played until the repair finishes."
            ),
        }
    }
}

#[derive(Debug)]
struct RiskyWork {
    game_id: String,
    op: RiskyOp,
}

fn risky_job(op_type: &str, kind: &str, job: Option<&Value>) -> Option<RiskyOp> {
    match op_type {
        "move" => return Some(RiskyOp::Move),
        "repair" => return Some(RiskyOp::Repair),
        _ => {}
    }
    let job = job?;
    if job["paused"] == Value::Bool(true) {
        return None;
    }
    match job["phase"].as_str().unwrap_or_default() {
        "extracting" => Some(RiskyOp::Unpack),
        "moving" => Some(RiskyOp::Move),
        "repairing" => Some(RiskyOp::Repair),
        "downloading" | "finalizing" if kind == "update" => Some(RiskyOp::Patch),
        _ => None,
    }
}

async fn unfinished_risky_work(app: &AppHandle) -> Option<RiskyWork> {
    if let Some(id) = game_profiles::GAME_IDS
        .iter()
        .find(|id| super::game_file_ops::is_uninstalling(id))
    {
        return Some(RiskyWork {
            game_id: (*id).to_string(),
            op: RiskyOp::Uninstall,
        });
    }
    let meta = app.state::<BackendState>().engine.queue.current_meta()?;
    let board = super::queue::get_state().await.unwrap_or(Value::Null);
    let job = board["jobs"]
        .as_array()
        .and_then(|jobs| jobs.iter().find(|j| j["gameId"] == meta.game_id.as_str()));
    risky_job(&meta.op_type, &meta.kind, job).map(|op| RiskyWork {
        game_id: meta.game_id,
        op,
    })
}

fn session_ending() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

pub(crate) async fn wait_for_renderer(app: &AppHandle) {
    const RENDERER_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);
    const RENDERER_POLL: std::time::Duration = std::time::Duration::from_millis(100);
    const RENDERER_SUBSCRIBE_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

    let started = std::time::Instant::now();
    loop {
        let signaled = app
            .try_state::<BackendState>()
            .map(|s| s.window.renderer_signaled())
            .unwrap_or(false);
        if signaled || started.elapsed() >= RENDERER_WAIT_LIMIT {
            break;
        }
        tokio::time::sleep(RENDERER_POLL).await;
    }
    tokio::time::sleep(RENDERER_SUBSCRIBE_GRACE).await;
}

// ------------ Window Commands ------------
// The small commands the frontend calls for its custom title bar buttons and the ready-to-show signal.
fn manager(app: &AppHandle) -> Arc<WindowManager> {
    app.state::<BackendState>().window.clone()
}

pub(super) async fn minimize_window(app: &AppHandle) -> Result<Value, String> {
    manager(app).minimize_window();
    Ok(ok_response())
}

pub(super) async fn close_window(app: &AppHandle) -> Result<Value, String> {
    if let Some(win) = main_window(app) {
        let _ = win.close();
    }
    Ok(ok_response())
}

pub(super) async fn toggle_maximize_window(app: &AppHandle) -> Result<Value, String> {
    let Some(win) = main_window(app) else {
        return Ok(ok_with(json!({ "maximized": false })));
    };
    let maximized = win.is_maximized().unwrap_or(false);
    let result = if maximized {
        win.unmaximize()
    } else {
        win.maximize()
    };
    if let Err(e) = result {
        log::warn!("[window] Failed to toggle maximize: {e}");
    }
    Ok(ok_with(
        json!({ "maximized": win.is_maximized().unwrap_or(!maximized) }),
    ))
}

pub(super) async fn get_window_state(app: &AppHandle) -> Result<Value, String> {
    let maximized = main_window(app)
        .map(|w| w.is_maximized().unwrap_or(false))
        .unwrap_or(false);
    Ok(ok_with(
        json!({ "maximized": maximized, "powerSave": is_backgrounded() }),
    ))
}

pub(super) async fn window_ready_to_show(app: &AppHandle) -> Result<Value, String> {
    let manager = manager(app);
    if manager.mark_renderer_signaled() {
        log::info!("ready-to-show: the renderer signalled again after a page reload.");
    } else {
        log::info!("startup: renderer ready at {} ms", super::logger::since_boot_ms());
    }
    manager.ready_to_show(ReadySource::Renderer);
    Ok(ok_response())
}

// ------------ Window Placement ------------
// Keeps the window on a real monitor, fits it to the work area, and picks an interface zoom for the monitor it lands on.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Bounds {
    fn scaled(self, scale: f64) -> Bounds {
        Bounds {
            x: self.x * scale,
            y: self.y * scale,
            w: self.w * scale,
            h: self.h * scale,
        }
    }

    fn overlap(self, other: Bounds) -> f64 {
        let w = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let h = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        if w > 0.0 && h > 0.0 {
            w * h
        } else {
            0.0
        }
    }

    fn clamped_into(self, area: Bounds) -> Bounds {
        let w = self.w.min(area.w);
        let h = self.h.min(area.h);
        Bounds {
            x: self.x.clamp(area.x, area.x + area.w - w),
            y: self.y.clamp(area.y, area.y + area.h - h),
            w,
            h,
        }
    }

    fn centered_in(self, area: Bounds) -> Bounds {
        let w = self.w.min(area.w);
        let h = self.h.min(area.h);
        Bounds {
            x: area.x + (area.w - w) / 2.0,
            y: area.y + (area.h - h) / 2.0,
            w,
            h,
        }
    }
}

fn landing_area(saved: Bounds, work_areas: &[(Bounds, f64)]) -> Option<(Bounds, f64)> {
    work_areas
        .iter()
        .map(|&(area, scale)| (saved.scaled(scale).overlap(area), area, scale))
        .filter(|(overlap, _, _)| *overlap > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, area, scale)| (area, scale))
}

fn place_saved_bounds(
    saved: Bounds,
    work_areas: &[(Bounds, f64)],
    fallback: Option<(Bounds, f64)>,
) -> Option<(Bounds, bool)> {
    if let Some((area, scale)) = landing_area(saved, work_areas) {
        return Some((saved.scaled(scale).clamped_into(area), false));
    }
    let (area, scale) = fallback?;
    Some((saved.scaled(scale).centered_in(area), true))
}

fn work_area_of(monitor: &tauri::Monitor) -> (Bounds, f64) {
    let work = monitor.work_area();
    (
        Bounds {
            x: f64::from(work.position.x),
            y: f64::from(work.position.y),
            w: f64::from(work.size.width),
            h: f64::from(work.size.height),
        },
        monitor.scale_factor(),
    )
}

fn place_on_monitors(win: &WebviewWindow, saved: Bounds) -> Option<(Bounds, bool)> {
    let monitors = match win.available_monitors() {
        Ok(monitors) => monitors,
        Err(e) => {
            log::warn!("display: could not list the monitors to place the window: {e}");
            return None;
        }
    };
    let areas: Vec<(Bounds, f64)> = monitors.iter().map(work_area_of).collect();
    let fallback = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| work_area_of(&monitor))
        .or_else(|| areas.first().copied());
    place_saved_bounds(saved, &areas, fallback)
}

fn drag_strip_visible(window: Bounds, areas: &[Bounds], strip: f64) -> bool {
    let top = Bounds {
        h: window.h.min(strip),
        ..window
    };
    areas.iter().any(|area| top.overlap(*area) > 0.0)
}

fn bring_on_screen(win: &WebviewWindow) {
    if win.is_minimized().unwrap_or(false)
        || win.is_maximized().unwrap_or(false)
        || win.is_fullscreen().unwrap_or(false)
    {
        return;
    }
    let (Ok(position), Ok(size), Ok(monitors)) =
        (win.outer_position(), win.outer_size(), win.available_monitors())
    else {
        return;
    };
    let window = Bounds {
        x: f64::from(position.x),
        y: f64::from(position.y),
        w: f64::from(size.width),
        h: f64::from(size.height),
    };
    let areas: Vec<Bounds> = monitors.iter().map(|m| work_area_of(m).0).collect();
    let strip = DRAG_STRIP_LOGICAL * win.scale_factor().unwrap_or(1.0);
    if areas.is_empty() || drag_strip_visible(window, &areas, strip) {
        return;
    }
    let fallback = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| (work_area_of(&monitor).0, 1.0))
        .or_else(|| areas.first().map(|&area| (area, 1.0)));
    let Some((placed, _)) = place_saved_bounds(window, &[], fallback) else {
        return;
    };
    log::info!(
        "display: the window title strip is off every monitor, moving the window to {},{}",
        placed.x,
        placed.y
    );
    let _ = win.set_position(tauri::PhysicalPosition::new(
        placed.x.round() as i32,
        placed.y.round() as i32,
    ));
    let _ = win.set_size(tauri::PhysicalSize::new(
        placed.w.round() as u32,
        placed.h.round() as u32,
    ));
}

const MIN_WINDOW_LOGICAL: (f64, f64) = (940.0, 560.0);
const WORK_AREA_FILL: f64 = 0.92;

const DESIGN_SIZE_LOGICAL: (f64, f64) = (1280.0, 720.0);
const SCREEN_SHARE: f64 = 0.5;
const UI_ZOOM_RANGE: (f64, f64) = (0.75, 1.5);
const UI_ZOOM_STEPS: f64 = 20.0;

fn ui_zoom(area: Bounds, scale: f64) -> f64 {
    let (design_w, design_h) = DESIGN_SIZE_LOGICAL;
    let by_width = area.w / scale * SCREEN_SHARE / design_w;
    let by_height = area.h / scale * WORK_AREA_FILL / design_h;
    let zoom = (by_width.min(by_height) * UI_ZOOM_STEPS).round() / UI_ZOOM_STEPS;
    zoom.clamp(UI_ZOOM_RANGE.0, UI_ZOOM_RANGE.1)
}

const UI_SCALE_RANGE: (f64, f64) = (0.5, 2.0);

fn parse_ui_scale(value: &Value) -> f64 {
    let percent = match value {
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    percent
        .filter(|p| p.is_finite() && *p > 0.0)
        .map_or(1.0, |p| (p / 100.0).clamp(UI_SCALE_RANGE.0, UI_SCALE_RANGE.1))
}

fn scaled_zoom(auto: f64, user: f64) -> f64 {
    (auto * user * 100.0).round() / 100.0
}

fn interface_zoom(area: Bounds, scale: f64, user: f64) -> f64 {
    let auto = ui_zoom(area, scale);
    let zoom = scaled_zoom(auto, user);
    if zoom <= auto {
        return zoom;
    }
    let (min_w, min_h) = MIN_WINDOW_LOGICAL;
    let fits = (area.w / scale / min_w).min(area.h / scale / min_h);
    zoom.min((fits * 100.0).floor() / 100.0).max(auto)
}

fn zoom_for_landing_monitor(win: &WebviewWindow, saved: Option<Bounds>, user: f64) -> f64 {
    let areas: Vec<(Bounds, f64)> = win
        .available_monitors()
        .map(|monitors| monitors.iter().map(work_area_of).collect())
        .unwrap_or_default();
    let primary = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| work_area_of(&monitor))
        .or_else(|| areas.first().copied());
    saved
        .and_then(|saved| landing_area(saved, &areas))
        .or(primary)
        .map_or(user, |(area, scale)| interface_zoom(area, scale, user))
}

fn lowered_min_size(area: Bounds, scale: f64, zoom: f64) -> Option<(f64, f64)> {
    let (min_w, min_h) = (MIN_WINDOW_LOGICAL.0 * zoom, MIN_WINDOW_LOGICAL.1 * zoom);
    let w = (area.w / scale).floor();
    let h = (area.h / scale).floor();
    (w < min_w || h < min_h).then_some((w.min(min_w), h.min(min_h)))
}

fn fitted_into(window: Bounds, area: Bounds, scale: f64, zoom: f64) -> Option<Bounds> {
    if window.w <= area.w && window.h <= area.h {
        return None;
    }
    let (min_w, min_h) = (MIN_WINDOW_LOGICAL.0 * zoom, MIN_WINDOW_LOGICAL.1 * zoom);
    let w = window
        .w
        .min((area.w * WORK_AREA_FILL).floor())
        .max((min_w * scale).min(area.w));
    let h = window
        .h
        .min((area.h * WORK_AREA_FILL).floor())
        .max((min_h * scale).min(area.h));
    Some(Bounds { w, h, ..window }.centered_in(area))
}

fn set_window_bounds(win: &WebviewWindow, b: Bounds) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let Ok(hwnd) = win.hwnd() else {
        return;
    };
    unsafe {
        SetWindowPos(
            hwnd.0 as windows_sys::Win32::Foundation::HWND,
            std::ptr::null_mut(),
            b.x.round() as i32,
            b.y.round() as i32,
            b.w.round() as i32,
            b.h.round() as i32,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn apply_scale_swap(win: &WebviewWindow, zoom: f64, b: Bounds) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let hwnd = win.hwnd().ok().map(|h| h.0 as isize);
    let _ = win.with_webview(move |webview| unsafe {
        if let Some(hwnd) = hwnd {
            SetWindowPos(
                hwnd as windows_sys::Win32::Foundation::HWND,
                std::ptr::null_mut(),
                b.x.round() as i32,
                b.y.round() as i32,
                b.w.round() as i32,
                b.h.round() as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let _ = webview.controller().SetZoomFactor(zoom);
    });
}

fn fit_to_work_area(win: &WebviewWindow, zoom: f64) {
    let monitor = match win.current_monitor() {
        Ok(Some(monitor)) => monitor,
        _ => match win.primary_monitor() {
            Ok(Some(monitor)) => monitor,
            _ => return,
        },
    };
    let (area, scale) = work_area_of(&monitor);
    if let Some((w, h)) = lowered_min_size(area, scale, zoom) {
        log::info!("display: work area is smaller than the minimum window size, lowering it to {w}x{h}");
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(w, h)));
    }
    let (Ok(position), Ok(size)) = (win.outer_position(), win.outer_size()) else {
        return;
    };
    let window = Bounds {
        x: f64::from(position.x),
        y: f64::from(position.y),
        w: f64::from(size.width),
        h: f64::from(size.height),
    };
    let Some(fitted) = fitted_into(window, area, scale, zoom) else {
        return;
    };
    log::info!(
        "display: window {}x{} does not fit the work area {}x{}, resizing to {}x{}",
        window.w,
        window.h,
        area.w,
        area.h,
        fitted.w,
        fitted.h
    );
    let _ = win.set_size(tauri::PhysicalSize::new(
        fitted.w.round() as u32,
        fitted.h.round() as u32,
    ));
    let _ = win.set_position(tauri::PhysicalPosition::new(
        fitted.x.round() as i32,
        fitted.y.round() as i32,
    ));
}

fn set_memory_low(win: &WebviewWindow, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface;

    WEBVIEW_HIDDEN.store(low, Ordering::SeqCst);
    let _ = win.with_webview(move |webview| unsafe {
        let controller = webview.controller();
        let _ = controller.SetIsVisible(!low);
        if let Ok(core) = controller.CoreWebView2() {
            if let Ok(core19) = core.cast::<ICoreWebView2_19>() {
                let level = if low {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                } else {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                };
                let _ = core19.SetMemoryUsageTargetLevel(level);
            }
        }
    });
}

// ------------ Window Manager Tests ------------
// Covers window placement, zoom, and which jobs ask before quitting.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_webviews_share_the_main_window_browser_args() {
        let config: Value = serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let main = config["app"]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["label"] == MAIN_WINDOW_LABEL)
            .unwrap();
        assert_eq!(
            main["additionalBrowserArgs"].as_str(),
            Some(WEBVIEW_BROWSER_ARGS)
        );
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Bounds {
        Bounds { x, y, w, h }
    }

    #[test]
    fn saved_bounds_on_a_monitor_are_kept() {
        let areas = [(rect(0.0, 0.0, 1920.0, 1040.0), 1.0)];
        let placed = place_saved_bounds(rect(100.0, 80.0, 1280.0, 720.0), &areas, Some(areas[0]));
        assert_eq!(placed, Some((rect(100.0, 80.0, 1280.0, 720.0), false)));
    }

    #[test]
    fn saved_bounds_hanging_off_the_edge_are_pulled_in() {
        let areas = [(rect(0.0, 0.0, 1920.0, 1040.0), 1.0)];
        let placed = place_saved_bounds(rect(1800.0, 900.0, 1280.0, 720.0), &areas, Some(areas[0]));
        assert_eq!(placed, Some((rect(640.0, 320.0, 1280.0, 720.0), false)));
    }

    #[test]
    fn saved_bounds_larger_than_the_monitor_shrink_to_fit() {
        let areas = [(rect(0.0, 0.0, 1366.0, 728.0), 1.0)];
        let placed = place_saved_bounds(rect(10.0, 10.0, 1920.0, 1080.0), &areas, Some(areas[0]));
        assert_eq!(placed, Some((rect(0.0, 0.0, 1366.0, 728.0), false)));
    }

    #[test]
    fn saved_bounds_on_a_disconnected_monitor_are_centered_on_the_primary() {
        let areas = [(rect(0.0, 0.0, 1920.0, 1040.0), 1.0)];
        let placed = place_saved_bounds(rect(-2400.0, 100.0, 1280.0, 720.0), &areas, Some(areas[0]));
        assert_eq!(placed, Some((rect(320.0, 160.0, 1280.0, 720.0), true)));
    }

    #[test]
    fn saved_bounds_use_the_scale_of_the_monitor_they_land_on() {
        let areas = [
            (rect(0.0, 0.0, 1920.0, 1040.0), 1.0),
            (rect(1920.0, 0.0, 3840.0, 2080.0), 2.0),
        ];
        let placed = place_saved_bounds(rect(1200.0, 100.0, 1280.0, 720.0), &areas, Some(areas[0]));
        assert_eq!(placed, Some((rect(2400.0, 200.0, 2560.0, 1440.0), false)));
    }

    #[test]
    fn no_monitors_means_no_placement() {
        assert_eq!(place_saved_bounds(rect(0.0, 0.0, 1280.0, 720.0), &[], None), None);
    }

    #[test]
    fn interface_size_multiplies_the_automatic_zoom() {
        assert_eq!(parse_ui_scale(&json!("100")), 1.0);
        assert_eq!(parse_ui_scale(&json!("125")), 1.25);
        assert_eq!(parse_ui_scale(&json!("900")), UI_SCALE_RANGE.1);
        assert_eq!(parse_ui_scale(&json!("junk")), 1.0);
        assert_eq!(parse_ui_scale(&Value::Null), 1.0);
        assert_eq!(scaled_zoom(1.15, 1.25), 1.44);
    }

    #[test]
    fn a_bigger_interface_size_still_fits_the_minimum_window() {
        let desktop = rect(0.0, 0.0, 2560.0, 1400.0);
        assert_eq!(interface_zoom(desktop, 1.0, 1.25), 1.25);
        let laptop = rect(0.0, 0.0, 1366.0, 728.0);
        assert_eq!(interface_zoom(laptop, 1.0, 0.8), 0.6);
        assert_eq!(interface_zoom(laptop, 1.0, 2.0), 1.3);
        let tiny = rect(0.0, 0.0, 640.0, 400.0);
        assert_eq!(interface_zoom(tiny, 1.0, 1.5), ui_zoom(tiny, 1.0));
    }

    #[test]
    fn minimum_window_size_matches_the_config() {
        let config: Value = serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let main = config["app"]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["label"] == MAIN_WINDOW_LABEL)
            .unwrap();
        assert_eq!(main["minWidth"].as_f64(), Some(MIN_WINDOW_LOGICAL.0));
        assert_eq!(main["minHeight"].as_f64(), Some(MIN_WINDOW_LOGICAL.1));
    }

    #[test]
    fn a_window_that_fits_the_work_area_is_left_alone() {
        let area = rect(0.0, 0.0, 1920.0, 1040.0);
        assert_eq!(fitted_into(rect(320.0, 160.0, 1280.0, 720.0), area, 1.0, 1.0), None);
        assert_eq!(lowered_min_size(area, 1.0, 1.0), None);
    }

    #[test]
    fn the_default_window_at_150_percent_shrinks_and_centers() {
        let area = rect(0.0, 0.0, 1920.0, 1000.0);
        let fitted = fitted_into(rect(0.0, -40.0, 1920.0, 1080.0), area, 1.5, 1.0);
        assert_eq!(fitted, Some(rect(77.0, 40.0, 1766.0, 920.0)));
        assert_eq!(lowered_min_size(area, 1.5, 1.0), None);
    }

    #[test]
    fn the_fitted_window_never_drops_below_the_minimum_size() {
        let area = rect(0.0, 0.0, 1920.0, 1032.0);
        let fitted = fitted_into(rect(-160.0, -114.0, 2240.0, 1260.0), area, 1.75, 1.0);
        assert_eq!(fitted, Some(rect(77.0, 26.0, 1766.0, 980.0)));
        assert_eq!(lowered_min_size(area, 1.75, 1.0), None);
    }

    #[test]
    fn a_window_whose_title_strip_is_on_a_monitor_stays_put() {
        let areas = [rect(0.0, 0.0, 1920.0, 1040.0)];
        assert!(drag_strip_visible(rect(1800.0, 1000.0, 1280.0, 720.0), &areas, 32.0));
        assert!(drag_strip_visible(rect(-1200.0, 10.0, 1280.0, 720.0), &areas, 32.0));
    }

    #[test]
    fn a_window_whose_title_strip_is_off_every_monitor_is_detected() {
        let areas = [rect(0.0, 0.0, 1920.0, 1040.0), rect(1920.0, 0.0, 1920.0, 1040.0)];
        assert!(!drag_strip_visible(rect(-2400.0, 100.0, 1280.0, 720.0), &areas, 32.0));
        assert!(!drag_strip_visible(rect(200.0, -600.0, 1280.0, 620.0), &areas, 48.0));
        assert!(!drag_strip_visible(rect(200.0, 100.0, 1280.0, 720.0), &[], 32.0));
    }

    fn board_job(phase: &str, paused: bool) -> Value {
        json!({ "gameId": "wuwa", "phase": phase, "paused": paused })
    }

    #[test]
    fn moves_and_repairs_always_ask_before_quitting() {
        assert_eq!(risky_job("move", "move", None), Some(RiskyOp::Move));
        assert_eq!(
            risky_job("repair", "repair", Some(&board_job("downloading", true))),
            Some(RiskyOp::Repair)
        );
    }

    #[test]
    fn unpacking_and_in_place_updates_ask_before_quitting() {
        let unpacking = board_job("extracting", false);
        assert_eq!(risky_job("download", "install", Some(&unpacking)), Some(RiskyOp::Unpack));
        let patching = board_job("downloading", false);
        assert_eq!(risky_job("download", "update", Some(&patching)), Some(RiskyOp::Patch));
        let finalizing = board_job("finalizing", false);
        assert_eq!(risky_job("download", "update", Some(&finalizing)), Some(RiskyOp::Patch));
        let repairing = board_job("repairing", false);
        assert_eq!(risky_job("verify", "verify", Some(&repairing)), Some(RiskyOp::Repair));
    }

    #[test]
    fn plain_downloads_and_idle_jobs_quit_without_asking() {
        let downloading = board_job("downloading", false);
        assert_eq!(risky_job("download", "install", Some(&downloading)), None);
        assert_eq!(risky_job("download", "update", Some(&board_job("queued", false))), None);
        assert_eq!(risky_job("download", "update", Some(&board_job("downloading", true))), None);
        assert_eq!(risky_job("download", "update", Some(&board_job("done", false))), None);
        assert_eq!(risky_job("download", "update", None), None);
        assert_eq!(risky_job("verify", "verify", Some(&board_job("scanning", false))), None);
    }

    #[test]
    fn a_work_area_below_the_minimum_lowers_it() {
        let area = rect(0.0, 0.0, 1920.0, 1032.0);
        assert_eq!(lowered_min_size(area, 2.0, 1.0), Some((940.0, 516.0)));
        let fitted = fitted_into(rect(-320.0, -204.0, 2560.0, 1440.0), area, 2.0, 1.0);
        assert_eq!(fitted, Some(rect(20.0, 0.0, 1880.0, 1032.0)));
    }

    #[test]
    fn a_4k_desktop_at_150_percent_keeps_the_designed_size() {
        assert_eq!(ui_zoom(rect(0.0, 0.0, 3840.0, 2088.0), 1.5), 1.0);
        assert_eq!(ui_zoom(rect(0.0, 0.0, 2560.0, 1392.0), 1.0), 1.0);
    }

    #[test]
    fn a_1080p_desktop_zooms_the_interface_out() {
        assert_eq!(ui_zoom(rect(0.0, 0.0, 1920.0, 1040.0), 1.0), 0.75);
        assert_eq!(ui_zoom(rect(0.0, 0.0, 1920.0, 1032.0), 1.25), 0.75);
        assert_eq!(ui_zoom(rect(0.0, 0.0, 1920.0, 1032.0), 1.5), 0.75);
    }

    #[test]
    fn large_desktops_zoom_the_interface_in_up_to_the_limit() {
        assert_eq!(ui_zoom(rect(0.0, 0.0, 3840.0, 2088.0), 1.25), 1.2);
        assert_eq!(ui_zoom(rect(0.0, 0.0, 3840.0, 2088.0), 1.0), 1.5);
        assert_eq!(ui_zoom(rect(0.0, 0.0, 7680.0, 4280.0), 1.0), 1.5);
    }

    #[test]
    fn a_short_desktop_zooms_by_its_height() {
        assert_eq!(ui_zoom(rect(0.0, 0.0, 3840.0, 1040.0), 1.0), 1.35);
    }

    #[test]
    fn the_minimum_size_shrinks_with_the_zoom() {
        let area = rect(0.0, 0.0, 1280.0, 700.0);
        assert_eq!(lowered_min_size(area, 1.5, 1.0), Some((853.0, 466.0)));
        assert_eq!(lowered_min_size(area, 1.5, 0.75), None);
    }

    #[test]
    fn the_zoom_follows_the_monitor_the_saved_window_lands_on() {
        let areas = [
            (rect(0.0, 0.0, 1920.0, 1040.0), 1.0),
            (rect(1920.0, 0.0, 3840.0, 2088.0), 1.5),
        ];
        let on_4k = landing_area(rect(1400.0, 100.0, 1280.0, 720.0), &areas).unwrap();
        assert_eq!(ui_zoom(on_4k.0, on_4k.1), 1.0);
        let on_1080p = landing_area(rect(100.0, 100.0, 1280.0, 720.0), &areas).unwrap();
        assert_eq!(ui_zoom(on_1080p.0, on_1080p.1), 0.75);
    }
}
