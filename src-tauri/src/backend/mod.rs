// ------------ Backend Root ------------
// Lists every backend module, routes each UI command by channel name (like "launch-game" or "list-mods") to the
// module that handles it, and holds the shared backend state and the small response helpers every module uses.

pub(crate) mod api_config;
pub(crate) mod bd2;
pub(crate) mod bluepoch;
pub(crate) mod dna;
pub(crate) mod capture;
pub(crate) mod config;
pub(crate) mod discord_audio;
pub(crate) mod engine;
pub(crate) mod extract;
pub(crate) mod file_channels;
pub(crate) mod fps_unlock;
pub(crate) mod fs_util;
mod game_file_ops;
pub(crate) mod game_manager;
pub mod game_profiles;
pub(crate) mod gamebanana;
pub(crate) mod gf2;
pub mod http;
pub(crate) mod hypergryph;
pub(crate) mod kuro;
pub mod logger;
pub(crate) mod manifest;
pub(crate) mod members;
pub(crate) mod mod_profiles;
pub(crate) mod mods;
mod news;
pub(crate) mod nte;
pub(crate) mod overlay;
pub(crate) mod platform;
pub(crate) mod playtime;
pub(crate) mod process_utils;
pub(crate) mod queue;
pub(crate) mod recorder;
pub(crate) mod sophon;
pub(crate) mod steam;
pub(crate) mod validator;
pub(crate) mod wallpaper_cache;
pub(crate) mod window_manager;
pub(crate) mod xxmi;
pub(crate) mod yostar;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use api_config::ApiConfig;
use config::LauncherConfig;
use file_channels::EnginePools;
use game_manager::{GameManager, GameUpdater};
use overlay::OverlayManager;
use wallpaper_cache::WallpaperCache;
use window_manager::WindowManager;

pub const BUILD_TYPE: &str = match option_env!("PEEBIFY_BUILD_TYPE") {
    Some(v) if matches!(v.as_bytes(), b"beta") => v,
    _ => "stable",
};

pub async fn dispatch(
    app: &AppHandle,
    channel: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    Some(match channel {
        "get-build-info" => Ok(json!({ "buildType": BUILD_TYPE })),

        "get-config" => config::get_config(app, args).await,
        "set-config" => config::set_config(app, args).await,
        "get-settings" => config::get_settings(app).await,
        "set-setting" => config::set_setting(app, args).await,
        "get-app-version" => config::get_app_version(app).await,
        "set-active-game" => config::set_active_game(app, args).await,
        "browse-game-path" => config::browse_game_path(app, args).await,
        "detect-default-game-path" => config::detect_default_game_path(app, args).await,
        "set-custom-launcher" => config::set_custom_launcher(app, args).await,
        "select-wallpaper-file" => config::select_media_file(app, "wallpaper").await,
        "select-game-icon-file" => config::select_media_file(app, "gameIcons").await,
        "save-game-icon" => config::save_media_slice(app, args, "gameIcons").await,
        "save-wallpaper" => config::save_media_slice(app, args, "wallpaper").await,
        "open-logs-folder" => config::open_logs_folder(app).await,
        "wipe-launcher-data" => config::wipe_launcher_data(app).await,

        "minimize-window" => window_manager::minimize_window(app).await,
        "close-window" => window_manager::close_window(app).await,
        "toggle-maximize-window" => window_manager::toggle_maximize_window(app).await,
        "get-window-state" => window_manager::get_window_state(app).await,
        "window-ready-to-show" => window_manager::window_ready_to_show(app).await,

        "open-external-url" => config::open_external_url(args).await,
        "get-game-links" => game_manager::get_game_links().await,
        "open-game-folder" => game_manager::open_game_folder(app, args).await,
        "open-screenshot-folder" => capture::open_screenshot_folder(app, args).await,
        "get-applied-voice-packs" => sophon::get_applied_voice_packs(app, args).await,

        "log-message" => Ok(logger::handle_log_message_channel(args)),

        "get-news-data" => news::get_news_data(app, args).await,

        "get-wallpaper-media" => wallpaper_cache::get_wallpaper_media(app).await,

        "get-network-status" => Ok(ok_with(json!({ "isOnline": http::is_online_cached() }))),
        "network-recheck" => http::network_recheck(app).await,

        "launch-game" => game_manager::launch_game(app, args).await,
        "get-running-game" => game_manager::get_running_game(app, args).await,
        "check-for-updates" => game_manager::check_for_updates(app, args).await,
        "get-install-preview" => engine::get_install_preview(app, args).await,
        "get-resource-quality" => kuro::get_resource_quality(app, args).await,
        "check-resource-quality" => kuro::check_resource_quality(app, args).await,
        "get-content-packs" => nte::get_content_packs(app, args).await,
        "discord-audio-clients" => discord_audio::list_audio_clients().await,
        "get-steam-install" => game_manager::get_steam_install(app, args).await,
        "get-fps-unlock" => fps_unlock::get_fps_unlock(app, args).await,
        "set-fps-unlock" => fps_unlock::set_fps_unlock(app, args).await,
        "get-playtime-data" => playtime::get_playtime_data(app).await,
        "get-playtime-sessions" => playtime::get_playtime_sessions(app, args).await,
        "playtime-delete-session" => playtime::delete_session(app, args).await,

        "get-download-queue-state" => queue::get_state().await,
        "start-download" => file_channels::start_download(app, args).await,
        "pause-download" => file_channels::pause_download(app, args).await,
        "resume-download" => file_channels::resume_download(app, args).await,
        "keep-going-while-playing" => file_channels::keep_going_while_playing(app, args).await,
        "cancel-download" => file_channels::cancel_download(app, args).await,
        "prioritize-download" => file_channels::prioritize_download(app, args).await,
        "start-repair" => file_channels::start_repair(app, args).await,
        "start-quick-repair" => file_channels::start_quick_repair(app, args).await,
        "cancel-repair" => file_channels::cancel_repair(app, args).await,
        "pause-repair" => file_channels::pause_repair(app, args).await,
        "resume-repair" => file_channels::resume_repair(app, args).await,
        "cancel-verify" => file_channels::cancel_verify(app, args).await,
        "cancel-move" => file_channels::cancel_move(app, args).await,
        "verify-game-integrity" => file_channels::verify_game_integrity(app, args).await,
        "select-install-directory" => file_channels::select_install_directory(app).await,
        "get-disk-space" => file_channels::get_disk_space(app, args).await,
        "get-game-storage" => file_channels::get_game_storage(app, args).await,
        "get-default-install-path" => file_channels::get_default_install_path(app, args).await,
        "get-install-path-health" => file_channels::get_install_path_health(app, args).await,
        "move-game-location" => file_channels::move_game_location(app, args).await,
        "uninstall-game" => file_channels::uninstall_game(app, args).await,
        "open-leftover-folder" => file_channels::open_leftover_folder(args).await,

        "mods-status" => mods::mods_status(app, args).await,
        "mod-ini-settings" => xxmi::mod_ini_settings(app, args).await,
        "set-mod-ini-setting" => xxmi::set_mod_ini_setting(app, args).await,
        "set-game-mods-enabled" => mods::set_game_mods_enabled(app, args).await,
        "list-mods" => mods::list_mods(app, args).await,
        "import-mod-archive" => mods::import_mod_archive(app, args).await,
        "set-mod-enabled" => mods::set_mod_enabled(app, args).await,
        "delete-mod" => mods::delete_mod(app, args).await,
        "open-mods-folder" => mods::open_mods_folder(app, args).await,
        "set-mods-path" => mods::set_mods_path(app, args).await,
        "set-mods-enabled-bulk" => mods::set_mods_enabled_bulk(app, args).await,

        "list-mod-profiles" => mod_profiles::list_mod_profiles(app, args).await,
        "create-mod-profile" => mod_profiles::create_mod_profile(app, args).await,
        "rename-mod-profile" => mod_profiles::rename_mod_profile(app, args).await,
        "delete-mod-profile" => mod_profiles::delete_mod_profile(app, args).await,
        "duplicate-mod-profile" => mod_profiles::duplicate_mod_profile(app, args).await,
        "set-mod-profile-members" => mod_profiles::set_mod_profile_members(app, args).await,
        "apply-mod-profile" => mod_profiles::apply_mod_profile(app, args).await,

        "get-overlay-config" => overlay::get_overlay_config(app).await,
        "set-overlay-hotkey" => overlay::set_overlay_hotkey(app, args).await,
        "set-overlay-capture-folder" => overlay::set_overlay_capture_folder(app, args).await,
        "open-capture-folder" => overlay::open_capture_folder(app).await,
        "get-overlay-status" => overlay::get_overlay_status(app).await,
        "overlay-screenshot" => capture::take_screenshot(app, args).await,
        "overlay-capture-status" => capture::capture_status(app).await,
        "overlay-list-captures" => capture::list_captures(app).await,
        "overlay-delete-capture" => capture::delete_capture(app, args).await,
        "overlay-reveal-capture" => capture::reveal_capture(app, args).await,
        "overlay-open-capture" => capture::open_capture(app, args).await,
        "overlay-record" => overlay::overlay_record(app).await,
        "suspend-overlay-hotkeys" => overlay::suspend_overlay_hotkeys(app, args).await,
        "overlay-toggle" => overlay::overlay_toggle(app, args).await,
        "overlay-open-launcher" => overlay::open_launcher(app).await,

        "gamebanana-feed" => gamebanana::feed(app, args).await,
        "gamebanana-categories" => gamebanana::categories(app, args).await,
        "gamebanana-mod-profile" => gamebanana::mod_profile(app, args).await,
        "install-gamebanana-mod" => gamebanana::install(app, args).await,
        "cancel-gamebanana-install" => gamebanana::cancel_install(app, args).await,
        "gamebanana-backfill-thumbnails" => gamebanana::backfill_thumbnails(app, args).await,
        "check-mod-updates" => gamebanana::check_updates(app, args).await,
        "update-gamebanana-mod" => gamebanana::update_mod(app, args).await,

        _ => return None,
    })
}

pub(crate) fn backend(app: &AppHandle) -> tauri::State<'_, BackendState> {
    app.state::<BackendState>()
}

pub(crate) fn active_game_id(app: &AppHandle) -> String {
    backend(app).config.active_game_id()
}

pub(crate) fn ok_response() -> Value {
    json!({ "success": true })
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn ok_with(data: Value) -> Value {
    let mut response = serde_json::Map::new();
    response.insert("success".to_string(), Value::Bool(true));
    if let Value::Object(map) = data {
        for (k, v) in map {
            response.insert(k, v);
        }
    }
    Value::Object(response)
}

pub(crate) fn err_response(error: impl Into<String>) -> Value {
    json!({ "success": false, "error": error.into() })
}

pub(crate) fn arg_str(args: &[Value], index: usize) -> Option<&str> {
    args.get(index)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

pub(crate) fn resolve_profile(app: &AppHandle, game_id: Option<&str>) -> &'static Value {
    let id = match game_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => app.state::<BackendState>().config.active_game_id(),
    };
    game_profiles::profile(&id)
}

pub(crate) fn wrap_result<T>(label: &str, outcome: Result<T, String>) -> Value {
    match outcome {
        Ok(_) => ok_response(),
        Err(e) => {
            log::error!("{label}: {e}");
            err_response(e)
        }
    }
}

// ------------ Backend State ------------
// The shared state every command can reach. Startup creates it: it finds the user data folder, sets up logging,
// loads the config and builds the managers for games, downloads, windows and the overlay.

pub struct BackendState {
    pub user_data: PathBuf,
    pub logs_dir: PathBuf,
    pub config: Arc<LauncherConfig>,
    pub api_config: Arc<ApiConfig>,
    pub wallpaper: Arc<WallpaperCache>,
    pub game: Arc<GameManager>,
    pub engine: Arc<EnginePools>,
    pub window: Arc<WindowManager>,
    pub game_updater: Arc<GameUpdater>,
    pub overlay: Arc<OverlayManager>,
}

impl BackendState {
    pub fn init(app: &AppHandle) -> Result<Self, String> {
        let user_data = match debug_user_data_override() {
            Some(dir) => dir,
            None => app
                .path()
                .data_dir()
                .map_err(|e| format!("could not resolve data dir: {e}"))?
                .join("Peebify Launcher"),
        };
        let logs_dir = user_data.join("logs");
        let _ = std::fs::create_dir_all(&user_data);
        let _ = std::fs::create_dir_all(&logs_dir);

        logger::init(&logs_dir, Some(platform::is_boot_launch()));

        let config = LauncherConfig::load(&user_data);
        config::apply_game_packs(&config);
        sophon::set_manifest_cache_dir(user_data.join("manifest-cache").join("sophon"));
        tauri::async_runtime::spawn_blocking(|| {
            sophon::sweep_manifest_cache(std::time::Duration::from_secs(60 * 60 * 24 * 14));
        });

        let now_ms = chrono::Utc::now().timestamp_millis();
        let install_roots: Vec<(&'static str, std::path::PathBuf)> =
            game_profiles::GAME_IDS
                .iter()
                .filter(|id| {
                    part_sweep_due(&config.get(&format!("games.{id}.lastPartSweep")), now_ms)
                })
                .filter_map(|id| match config.get(&format!("games.{id}.gamePath")) {
                    serde_json::Value::String(path) if !path.trim().is_empty() => {
                        Some((*id, std::path::PathBuf::from(path)))
                    }
                    _ => None,
                })
                .collect();
        let sweep_config = Arc::clone(&config);
        tauri::async_runtime::spawn_blocking(move || {
            for (id, root) in install_roots {
                validator::sweep_orphaned_parts(
                    &root,
                    std::time::Duration::from_secs(60 * 60 * 24 * 7),
                );
                sweep_config.set(
                    &format!("games.{id}.lastPartSweep"),
                    serde_json::json!(chrono::Utc::now().timestamp_millis()),
                );
            }
        });
        let api_config = ApiConfig::new(&user_data);
        let wallpaper = WallpaperCache::new(app.clone(), &user_data);
        let game = GameManager::new(app.clone());
        let engine = EnginePools::new(app.clone());
        let window = WindowManager::new(app.clone());
        let game_updater = GameUpdater::new(app.clone());
        let overlay = OverlayManager::new(app.clone());
        let transfer_settings = config::transfer_settings(&config.get("behavior"));
        tauri::async_runtime::spawn_blocking(move || {
            http::shared_transfer().apply_settings(transfer_settings);
        });

        Ok(Self {
            user_data,
            logs_dir,
            config,
            api_config,
            wallpaper,
            game,
            engine,
            window,
            game_updater,
            overlay,
        })
    }

    pub fn allow_asset_paths(&self, app: &AppHandle) {
        fs_util::allow_asset_dir(app, &self.user_data.join("wallpaper-cache"));
        config::allow_custom_media(app, &self.config);
        let app = app.clone();
        let config = Arc::clone(&self.config);
        tauri::async_runtime::spawn_blocking(move || {
            let captures = overlay::capture_dir_in(&app, &config);
            if captures.is_dir() {
                fs_util::allow_asset_dir(&app, &captures);
            } else {
                log::info!(
                    "asset scope: the capture folder {} is not reachable yet",
                    captures.display()
                );
            }
        });
    }
}

const PART_SWEEP_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

fn part_sweep_due(last: &serde_json::Value, now_ms: i64) -> bool {
    match last.as_i64() {
        Some(last) if last <= now_ms => now_ms - last >= PART_SWEEP_INTERVAL_MS,
        _ => true,
    }
}

pub(crate) fn user_data_dir_before_setup() -> Option<std::path::PathBuf> {
    debug_user_data_override().or_else(|| {
        std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("Peebify Launcher"))
    })
}

fn debug_user_data_override() -> Option<std::path::PathBuf> {
    #[cfg(debug_assertions)]
    {
        let raw = std::env::var("PEEBIFY_USER_DATA").ok()?;
        let path = std::path::PathBuf::from(raw.trim());
        if path.is_absolute() {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_sweep_waits_for_interval() {
        let now = 10 * PART_SWEEP_INTERVAL_MS;
        assert!(part_sweep_due(&serde_json::Value::Null, now));
        assert!(!part_sweep_due(&serde_json::json!(now - 1), now));
        assert!(!part_sweep_due(
            &serde_json::json!(now - PART_SWEEP_INTERVAL_MS + 1),
            now
        ));
        assert!(part_sweep_due(
            &serde_json::json!(now - PART_SWEEP_INTERVAL_MS),
            now
        ));
    }
}
