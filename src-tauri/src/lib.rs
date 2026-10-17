#![recursion_limit = "256"]

// ------------ Launcher Entry Point ------------
// Where the launcher boots. Sets up logging and the startup crash dialog, keeps a single copy running,
// builds the backend state and starts its background jobs. The web UI talks to all of it through the
// one `rpc` command. Windows only.

#[cfg(not(windows))]
compile_error!("Peebify Launcher only builds for Windows");

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

mod backend;

use backend::BackendState;

#[tauri::command]
async fn rpc(app: AppHandle, channel: String, args: Option<Vec<Value>>) -> Result<Value, String> {
    let args_vec = args.unwrap_or_default();
    let started = std::time::Instant::now();
    let result = match backend::dispatch(&app, &channel, &args_vec).await {
        Some(result) => result,
        None => Err(format!("Unknown rpc channel: {channel}")),
    };
    backend::logger::record_rpc(&channel, started.elapsed(), &result);
    result
}

struct ShellLogger;

impl log::Log for ShellLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ours = record.target().starts_with("peebify_launcher_lib");
        if record.level() <= log::Level::Info || ours {
            eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
        }
        if ours {
            backend::logger::append(record.level().as_str(), &record.args().to_string());
        } else if record.level() <= log::Level::Warn {
            backend::logger::append(
                record.level().as_str(),
                &format!("[{}] {}", record.target(), record.args()),
            );
        }
    }

    fn flush(&self) {}
}

static SHELL_LOGGER: ShellLogger = ShellLogger;

const QUIT_FOR_UPDATE_FLAG: &str = "--quit-for-update";
const BUILD_FAILED_MSG: &str = "error while building tauri application";
const SETUP_FAILED_MSG: &str = "Failed to setup app";
const MAX_DIALOG_DETAIL_CHARS: usize = 600;

static STARTUP_FAILURE_DIALOG: AtomicBool = AtomicBool::new(false);

fn quit_for_update_requested() -> bool {
    std::env::args().any(|a| a == QUIT_FOR_UPDATE_FLAG)
}

fn startup_failure_text(message: &str) -> String {
    let mut text = String::from(
        "Peebify couldn't start. If this keeps happening, reinstall Microsoft Edge WebView2 or run the Peebify installer again.",
    );
    if message.contains(SETUP_FAILED_MSG) || message.contains(BUILD_FAILED_MSG) {
        let detail: String = message.chars().take(MAX_DIALOG_DETAIL_CHARS).collect();
        text.push_str("\n\nDetails: ");
        text.push_str(&detail);
    }
    text
}

fn show_startup_failure(text: String) {
    fn message_box(text: &str) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
        };
        use peebify_helpers::common::wide;
        let body = wide(text);
        let title = wide("Peebify Launcher");
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
            );
        }
    }
    let fallback = text.clone();
    match std::thread::Builder::new()
        .name("startup-failure".to_string())
        .spawn(move || message_box(&text))
    {
        Ok(handle) => {
            let _ = handle.join();
        }
        Err(_) => message_box(&fallback),
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with a non-string payload".to_string());

        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("unnamed").to_string();

        backend::logger::append(
            "crash",
            &format!(
                "panic in thread '{thread_name}' at {location}: {message}\n{}",
                std::backtrace::Backtrace::force_capture()
            ),
        );

        let fatal = cfg!(panic = "abort") || thread_name == "main";
        if fatal && STARTUP_FAILURE_DIALOG.swap(false, Ordering::SeqCst) {
            show_startup_failure(startup_failure_text(&message));
        }

        previous(info);
    }));
}

pub fn run() {
    backend::logger::mark_boot();
    if log::set_logger(&SHELL_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    install_panic_hook();

    if !quit_for_update_requested() {
        STARTUP_FAILURE_DIALOG.store(true, Ordering::SeqCst);
        backend::platform::register_app_id();
    }

    tauri::Builder::default()
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let Some(state) = app.try_state::<BackendState>() else {
                return;
            };
            if argv.iter().any(|a| a == QUIT_FOR_UPDATE_FLAG) {
                log::info!("quit requested by setup");
                state.window.quit_app();
            } else {
                state.window.show_window();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            if quit_for_update_requested() {
                app.handle().cleanup_before_exit();
                std::process::exit(0);
            }

            let app_handle = app.handle().clone();

            backend::platform::allow_full_speed();
            backend::platform::watch_suspend_resume();

            let backend_state = BackendState::init(&app_handle)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            backend_state.allow_asset_paths(&app_handle);
            app.manage(backend_state);

            let state = app.state::<BackendState>();

            state.window.wire_menu_handler();
            state.window.restore_window_state();
            state.window.log_display_environment();

            {
                let wallpaper = state.wallpaper.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    wallpaper.load_from_disk();
                    wallpaper.start();
                });
                backend::xxmi::boot(&app_handle);
                state.game_updater.start();
                state.game.resume_open_sessions();
                backend::http::start_monitoring(&app_handle);
            }

            {
                let config = state.config.clone();
                let app = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    let game_paths = |config: &backend::config::LauncherConfig| -> Vec<Value> {
                        backend::game_profiles::GAME_IDS
                            .iter()
                            .map(|id| config.get(&format!("games.{id}.gamePath")))
                            .collect()
                    };
                    let paths_before = game_paths(&config);
                    let had_moves = backend::game_profiles::GAME_IDS.iter().any(|id| {
                        config.get(&format!("games.{id}.pendingMove")).is_object()
                    });
                    let sanitizing = config.clone();
                    let sanitized = tauri::async_runtime::spawn_blocking(move || {
                        sanitizing.sanitize_installed_paths_deferred()
                    })
                    .await
                    .unwrap_or(false);
                    let adopted =
                        backend::file_channels::adopt_default_location_installs(&app).await;
                    let resanitized = had_moves && {
                        let sanitizing = config.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            sanitizing.sanitize_installed_paths_deferred()
                        })
                        .await
                        .unwrap_or(false)
                    };
                    let paths_changed =
                        sanitized || resanitized || game_paths(&config) != paths_before;
                    if !adopted.is_empty() || paths_changed {
                        backend::window_manager::wait_for_renderer(&app).await;
                        let _ = app.emit("games-detected", json!({ "gameIds": adopted }));
                    }
                });
            }

            {
                let api_config = state.api_config.clone();
                tauri::async_runtime::spawn(async move {
                    api_config.start_periodic_refresh();
                    if let Err(e) = api_config.initialize().await {
                        log::warn!(
                            "API config bootstrap failed (news and wallpaper slogans may be unavailable): {e}"
                        );
                    }
                });
            }

            {
                let window_manager = state.window.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    window_manager.ready_to_show(backend::window_manager::ReadySource::Backstop);
                });
            }

            let app_for_failsafe = app_handle.clone();
            let window_for_failsafe = state.window.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                let hidden_in_tray = window_for_failsafe.tray_active();
                match app_for_failsafe.get_webview_window("main") {
                    Some(win) => {
                        if win.is_visible().unwrap_or(false) {
                            log::debug!("window visible within 8s grace period");
                        } else if hidden_in_tray {
                            log::info!("window hidden in the tray by choice — failsafe skipped");
                        } else {
                            window_for_failsafe.show_window();
                            log::error!(
                                "FAILSAFE: main window still hidden after 8s — forced show()"
                            );
                        }
                    }
                    None => log::error!("FAILSAFE: main window 'main' not found after 8s"),
                }
            });

            STARTUP_FAILURE_DIALOG.store(false, Ordering::SeqCst);
            log::info!("startup: setup done at {} ms", backend::logger::since_boot_ms());
            Ok(())
        })
        .on_window_event(|window, event| {
            use backend::overlay::{DRAWER_LABEL, HUD_LABEL};
            use tauri::WindowEvent;
            let label = window.label();
            if label == DRAWER_LABEL || label == HUD_LABEL {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    let Some(state) = window.app_handle().try_state::<BackendState>() else {
                        return;
                    };
                    if state.window.is_quitting() {
                        return;
                    }
                    api.prevent_close();
                    if label == DRAWER_LABEL {
                        state.overlay.close_drawer(true);
                    }
                }
                return;
            }
            if label != "main" {
                return;
            }
            let app = window.app_handle().clone();
            let window_manager = app.try_state::<BackendState>().map(|s| s.window.clone());

            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    let prevent = window_manager
                        .map(|w| w.handle_close_requested())
                        .unwrap_or(false);
                    if prevent {
                        api.prevent_close();
                    }
                }
                WindowEvent::Destroyed => {
                    if let Some(state) = app.try_state::<BackendState>() {
                        state.config.flush();
                    }
                    backend::logger::flush();
                    if let Some(wm) = window_manager {
                        wm.on_main_window_destroyed();
                    }
                }
                WindowEvent::Resized(_) => {
                    if let Some(wm) = window_manager {
                        wm.on_window_resized();
                        wm.schedule_window_state_save();
                    }
                }
                WindowEvent::Moved(_) => {
                    if let Some(wm) = window_manager {
                        wm.schedule_zoom_check();
                        wm.schedule_window_state_save();
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![rpc])
        .build(tauri::generate_context!())
        .expect(BUILD_FAILED_MSG)
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<BackendState>() {
                    state.window.finish_recording_before_exit();
                }
                backend::logger::end_session("exit");
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_failure_text_adds_detail_only_for_tauri_startup_errors() {
        let plain = startup_failure_text("index out of bounds");
        assert!(plain.starts_with("Peebify couldn't start."));
        assert!(!plain.contains("Details:"));

        let setup = startup_failure_text("Failed to setup app: could not resolve data dir");
        assert!(setup.ends_with("Details: Failed to setup app: could not resolve data dir"));

        let build = startup_failure_text(&format!("{BUILD_FAILED_MSG}: {}", "x".repeat(2000)));
        let detail = build.split("Details: ").nth(1).unwrap();
        assert_eq!(detail.chars().count(), MAX_DIALOG_DETAIL_CHARS);
    }
}
