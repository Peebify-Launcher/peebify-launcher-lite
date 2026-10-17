// ------------ Wallpaper Cache ------------
// Keeps each game's launcher background (image or video) downloaded in the user data folder and checked against the
// server's hashes, syncing in the background so wallpapers still show when offline.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use parking_lot::RwLock;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;

use super::api_config::ApiConfig;
use super::BackendState;
use super::{game_profiles, http};

fn server_wallpaper_dir(game_id: &str) -> &str {
    game_profiles::known_profile(game_id)
        .and_then(|profile| profile.get("wallpaperSlug"))
        .and_then(Value::as_str)
        .unwrap_or(game_id)
}

fn remote_fields_for(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "static" => ("staticFile", "staticFileHash"),
        _ => ("backgroundFile", "backgroundFileHash"),
    }
}

const STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(8);
const WAKE_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);
const HIDDEN_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const HIDDEN_AT: &str = "hiddenAt";
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const DOWNLOAD_RETRIES: u32 = 2;
const DOWNLOAD_RETRY_BASE_MS: u64 = 2000;
const STATE_FILE: &str = "state.json";
const KINDS: [&str; 2] = ["wallpaper", "static"];
const VIDEO_EXTENSIONS: [&str; 4] = [".mp4", ".webm", ".mov", ".m4v"];

pub(super) fn is_video_ext(ext: &str) -> bool {
    VIDEO_EXTENSIONS
        .iter()
        .any(|video| video.eq_ignore_ascii_case(ext))
}

const CACHEABLE_EXTENSIONS: [&str; 9] = [
    ".mp4", ".webm", ".mov", ".m4v", ".png", ".jpg", ".jpeg", ".webp", ".gif",
];
const MAX_WALLPAPER_BYTES: u64 = 512 * 1024 * 1024;

fn wallpaper_ext(remote_file: &str, kind: &str) -> Option<String> {
    let parsed = url::Url::parse(remote_file).ok()?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return None;
    }
    let fallback = if kind == "static" { ".webp" } else { ".mp4" };
    let ext = Path::new(parsed.path())
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
        .filter(|e| CACHEABLE_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or_else(|| fallback.to_string());
    Some(ext)
}

fn content_named_path(dir: &Path, kind: &str, ext: &str, hash: &str) -> Option<PathBuf> {
    let short = hash.get(..8)?;
    Some(dir.join(format!("{kind}-{short}{ext}")))
}

fn parse_state(text: &str) -> Result<Value, String> {
    let state: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if state.is_object() {
        Ok(state)
    } else {
        Err("not a JSON object".to_string())
    }
}

fn sync_order(active_id: &str, visible: &Value, setup_complete: bool) -> Vec<&'static str> {
    let listed: Vec<&str> = if setup_complete {
        visible
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut order: Vec<&'static str> = game_profiles::GAME_IDS
        .iter()
        .copied()
        .filter(|id| *id == active_id)
        .collect();
    order.extend(
        game_profiles::GAME_IDS
            .iter()
            .copied()
            .filter(|id| *id != active_id && listed.contains(id)),
    );
    order
}

#[derive(Debug, PartialEq, Eq)]
enum HiddenCache {
    Mark,
    Keep,
    Remove,
}

fn hidden_cache_action(state: &Value, now_ms: i64) -> HiddenCache {
    match state.get(HIDDEN_AT).and_then(Value::as_i64) {
        Some(at) if at <= now_ms && now_ms - at >= HIDDEN_GRACE_MS => HiddenCache::Remove,
        Some(at) if at <= now_ms => HiddenCache::Keep,
        _ => HiddenCache::Mark,
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

#[derive(Default)]
struct GameSync {
    changed: bool,
    downloads: Vec<String>,
    failures: Vec<String>,
}

pub struct WallpaperCache {
    app: AppHandle,
    cache_root: PathBuf,
    media: RwLock<Map<String, Value>>,
    loaded: AtomicBool,
    refresh_lock: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
}

impl WallpaperCache {
    pub fn new(app: AppHandle, user_data: &Path) -> Arc<Self> {
        Arc::new(Self {
            app,
            cache_root: user_data.join("wallpaper-cache"),
            media: RwLock::new(Map::new()),
            loaded: AtomicBool::new(false),
            refresh_lock: tokio::sync::Mutex::new(()),
            wake: tokio::sync::Notify::new(),
        })
    }

    fn cache_dir_for(&self, game_id: &str) -> PathBuf {
        self.cache_root.join(game_id)
    }

    pub fn wake(&self, reason: &str) {
        log::info!("Wallpaper cache: sync requested {reason}");
        self.wake.notify_one();
    }

    pub fn wake_if_uncached(&self, game_id: &str) {
        if !self.cache_dir_for(game_id).is_dir() {
            self.wake(&format!("because {game_id} has no cache yet"));
        }
    }

    fn animations_disabled(&self) -> bool {
        self.app.try_state::<BackendState>().is_some_and(|s| {
            matches!(
                s.config.get("behavior.disableAnimations"),
                Value::Bool(true)
            )
        })
    }

    fn load_state(&self, game_id: &str) -> Value {
        let state_file = self.cache_dir_for(game_id).join(STATE_FILE);
        match std::fs::read_to_string(&state_file) {
            Ok(text) => parse_state(&text).unwrap_or_else(|e| {
                log::warn!("Wallpaper cache state unreadable for {game_id}: {e}");
                json!({})
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(e) => {
                log::warn!("Wallpaper cache state unreadable for {game_id}: {e}");
                json!({})
            }
        }
    }

    fn save_state(&self, game_id: &str, state: &Value) -> Result<(), String> {
        let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
        super::fs_util::write_atomic(&self.cache_dir_for(game_id).join(STATE_FILE), text.as_bytes())
    }

    pub fn load_from_disk(&self) {
        let mut media = Map::new();
        for game_id in game_profiles::GAME_IDS {
            let state = self.load_state(game_id);
            media.insert(game_id.to_string(), self.entry_for(game_id, &state));
        }
        *self.media.write() = media;
        self.loaded.store(true, Ordering::SeqCst);
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    pub fn media_map(&self) -> Value {
        Value::Object(self.media.read().clone())
    }

    fn wallpaper_json_url(&self, game_id: &str, api_config: &ApiConfig) -> String {
        if let Some(url) = api_config.wallpapers_slogan_url_for_client(Some(game_id)) {
            return url;
        }
        let origin = super::api_config::api_origin();
        format!(
            "{origin}/launcher/wallpaper/{}/wallpapers-slogan.json",
            server_wallpaper_dir(game_id)
        )
    }

    async fn refresh_game(&self, game_id: &str, api_config: &ApiConfig) -> Result<GameSync, String> {
        let url = self.wallpaper_json_url(game_id, api_config);

        let dir = self.cache_dir_for(game_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut state = self.load_state(game_id);
        let mut sync = GameSync::default();

        if let Some(map) = state.as_object_mut() {
            if map.remove(HIDDEN_AT).is_some() {
                self.save_state(game_id, &state)?;
            }
        }

        let known = state
            .get("feedEtag")
            .and_then(|v| v.as_str())
            .filter(|e| !e.is_empty())
            .map(str::to_string);

        let (body, feed_etag) = match http::get_text_conditional(&url, known.as_deref()).await? {
            http::Conditional::Unchanged => {
                log::debug!("{game_id}: wallpaper feed unchanged (304)");
                return Ok(sync);
            }
            http::Conditional::Fresh { text, etag } => (text, etag),
        };

        let remote: Value = serde_json::from_str(&body)
            .map_err(|e| format!("Invalid wallpaper JSON from {url}: {e}"))?;

        let etag_moved = feed_etag.is_some() && feed_etag != known;

        let mut changed = false;
        let mut skipped = false;

        let animations_disabled = self.animations_disabled();

        for kind in KINDS {
            let (url_field, hash_field) = remote_fields_for(kind);
            let remote_file = remote
                .get(url_field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let Some(remote_file) = remote_file else {
                if state.get(kind).is_some_and(|v| !v.is_null()) {
                    if let Some(map) = state.as_object_mut() {
                        map.remove(kind);
                    }
                    changed = true;
                }
                continue;
            };
            let server_hash = remote
                .get(hash_field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let prev = &state[kind];
            let prev_file = prev
                .get("file")
                .and_then(|v| v.as_str())
                .map(|name| dir.join(name));
            let up_to_date = match &prev_file {
                Some(path) if path.exists() => match &server_hash {
                    Some(hash) => prev.get("hash").and_then(|v| v.as_str()) == Some(hash),
                    None => prev.get("url").and_then(|v| v.as_str()) == Some(&remote_file),
                },
                _ => false,
            };
            if up_to_date {
                continue;
            }

            let Some(ext) = wallpaper_ext(&remote_file, kind) else {
                log::warn!("Wallpaper cache: {game_id} {kind} skipped, {remote_file} is not an http(s) URL");
                sync.failures.push(format!("{kind}: not an http(s) URL"));
                continue;
            };

            if kind == "wallpaper" && animations_disabled && is_video_ext(&ext) {
                log::debug!("{game_id}: animated wallpaper skipped by user setting");
                skipped = true;
                continue;
            }

            let on_disk = server_hash.as_deref().and_then(|hash| {
                content_named_path(&dir, kind, &ext, hash)
                    .filter(|path| path.is_file())
                    .map(|path| (path, hash.to_string()))
            });
            let fetched = match on_disk {
                Some((path, hash)) => Ok((path, hash, None)),
                None => {
                    log::info!("Wallpaper cache: downloading {game_id} {kind} from {remote_file}");
                    let started = std::time::Instant::now();
                    let label = format!("Wallpaper download for {game_id} {kind}");
                    http::with_retry(
                        || download_verified(&remote_file, &dir, kind, &ext, server_hash.as_deref()),
                        1 + DOWNLOAD_RETRIES,
                        DOWNLOAD_RETRY_BASE_MS,
                        &label,
                    )
                    .await
                    .map(|(file, hash, bytes)| (file, hash, Some((bytes, started.elapsed()))))
                }
            };
            let (file, hash, transfer) = match fetched {
                Ok(fetched) => fetched,
                Err(e) => {
                    log::warn!("Wallpaper cache: {game_id} {kind} failed: {e}");
                    sync.failures.push(format!("{kind}: {e}"));
                    continue;
                }
            };
            let file_name = file
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            state[kind] = json!({ "file": file_name, "hash": hash, "url": remote_file });
            changed = true;
            match transfer {
                Some((bytes, elapsed)) => {
                    let detail = format!(
                        "{} in {:.1} s",
                        megabytes(bytes),
                        elapsed.as_secs_f64()
                    );
                    log::info!("Wallpaper cache: {game_id} {kind} cached as {file_name} ({detail})");
                    sync.downloads.push(format!("{kind} {detail}"));
                }
                None => {
                    log::info!("Wallpaper cache: {game_id} {kind} reused {file_name} already on disk");
                }
            }
        }

        let complete = !skipped && sync.failures.is_empty();
        if complete {
            if let Some(fresh) = &feed_etag {
                state["feedEtag"] = json!(fresh);
            }
        }

        if changed {
            state["lastSync"] =
                json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            self.save_state(game_id, &state)?;
            self.prune_game_cache(game_id, &state);
            self.media
                .write()
                .insert(game_id.to_string(), self.entry_for(game_id, &state));
        } else if etag_moved && complete {
            self.save_state(game_id, &state)?;
        }
        sync.changed = changed;
        Ok(sync)
    }

    fn entry_for(&self, game_id: &str, state: &Value) -> Value {
        let dir = self.cache_dir_for(game_id);
        let mut entry = Map::new();
        for kind in KINDS {
            let value = state[kind]
                .get("file")
                .and_then(|v| v.as_str())
                .map(|name| dir.join(name))
                .filter(|path| path.exists())
                .map(|path| Value::String(path.to_string_lossy().to_string()))
                .unwrap_or(Value::Null);
            entry.insert(kind.to_string(), value);
        }
        Value::Object(entry)
    }

    fn prune_game_cache(&self, game_id: &str, state: &Value) {
        let dir = self.cache_dir_for(game_id);
        let mut keep: Vec<String> = vec![STATE_FILE.to_string()];
        for kind in KINDS {
            if let Some(name) = state[kind].get("file").and_then(|v| v.as_str()) {
                keep.push(name.to_string());
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if keep.iter().any(|k| k == &name) {
                continue;
            }
            if let Err(e) = std::fs::remove_file(entry.path()) {
                log::warn!("Wallpaper cache prune failed for {game_id}/{name}: {e}");
            }
        }
    }

    fn prune_unsynced_games(&self, keep: &[&str]) -> bool {
        let Ok(entries) = std::fs::read_dir(&self.cache_root) else {
            return false;
        };
        let now = super::now_ms();
        let mut pruned = false;
        for entry in entries.filter_map(|e| e.ok()) {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if keep.contains(&name.as_str()) {
                continue;
            }
            if game_profiles::GAME_IDS.contains(&name.as_str()) {
                let mut state = self.load_state(&name);
                match hidden_cache_action(&state, now) {
                    HiddenCache::Keep => continue,
                    HiddenCache::Mark => {
                        state[HIDDEN_AT] = json!(now);
                        match self.save_state(&name, &state) {
                            Ok(()) => log::info!(
                                "Wallpaper cache: {name} is not in the library, its cache is kept for a while in case it comes back"
                            ),
                            Err(e) => log::warn!("Wallpaper cache: could not mark {name} as hidden: {e}"),
                        }
                        continue;
                    }
                    HiddenCache::Remove => {}
                }
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    log::info!("Wallpaper cache: removed the cache for {name}, it is not in the library");
                    if game_profiles::GAME_IDS.contains(&name.as_str()) {
                        let empty = self.entry_for(&name, &json!({}));
                        self.media.write().insert(name, empty);
                    }
                    pruned = true;
                }
                Err(e) => log::warn!("Wallpaper cache: could not remove the cache for {name}: {e}"),
            }
        }
        pruned
    }

    pub async fn refresh_all(self: &Arc<Self>) -> bool {
        let Ok(_guard) = self.refresh_lock.try_lock() else {
            let _wait = self.refresh_lock.lock().await;
            return false;
        };

        if !super::http::is_online_cached() {
            log::info!("Wallpaper cache: offline, serving the existing cache.");
            return true;
        }
        if !super::api_config::peebify_content_on(&self.app) {
            log::info!("Wallpaper cache: Peebify online content is off, serving the existing cache.");
            return false;
        }

        let started = std::time::Instant::now();
        let state = self.app.state::<BackendState>();
        let api_config = state.api_config.clone();
        let active_id = state.config.active_game_id();
        let visible = state.config.get("library.visible");
        let setup_complete = matches!(state.config.get("library.setupComplete"), Value::Bool(true));
        let order = sync_order(&active_id, &visible, setup_complete);

        let pruned = setup_complete
            && visible.as_array().is_some_and(|ids| !ids.is_empty())
            && self.prune_unsynced_games(&order);

        let mut any_changed = false;
        let mut unchanged = 0usize;
        let mut updated: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for game_id in &order {
            match self.refresh_game(game_id, &api_config).await {
                Ok(sync) => {
                    if sync.changed {
                        any_changed = true;
                        self.notify_renderer();
                        if sync.downloads.is_empty() {
                            updated.push(game_id.to_string());
                        } else {
                            updated.push(format!("{game_id} {}", sync.downloads.join(", ")));
                        }
                    } else if sync.failures.is_empty() {
                        unchanged += 1;
                    }
                    if !sync.failures.is_empty() {
                        failed.push(format!("{game_id}: {}", sync.failures.join("; ")));
                    }
                }
                Err(e) => {
                    log::warn!("Wallpaper cache sync failed for {game_id}: {e}");
                    failed.push(format!("{game_id}: {e}"));
                }
            }
        }
        if pruned && !any_changed {
            self.notify_renderer();
        }
        let any_failed = !failed.is_empty();
        let list = |items: &[String]| {
            if items.is_empty() {
                String::new()
            } else {
                format!(" ({})", items.join("; "))
            }
        };
        log::info!(
            "Wallpaper cache sync: {} games, {unchanged} unchanged, {} updated{}, {} failed{} in {:.1} s{}.",
            order.len(),
            updated.len(),
            list(&updated),
            failed.len(),
            list(&failed),
            started.elapsed().as_secs_f64(),
            if any_failed { ", retrying sooner" } else { "" }
        );
        any_failed
    }

    fn notify_renderer(&self) {
        if let Err(e) = self.app.emit(
            "wallpaper-media-updated",
            json!({ "media": self.media_map() }),
        ) {
            log::warn!("Wallpaper cache: renderer notify failed: {e}");
        }
    }

    async fn wait_for_sync(&self, pause: std::time::Duration) {
        let woken = tokio::select! {
            _ = tokio::time::sleep(pause) => false,
            _ = self.wake.notified() => true,
        };
        if woken {
            tokio::time::sleep(WAKE_SETTLE).await;
        }
        let _ = self.wake.notified().now_or_never();
    }

    pub fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            me.wait_for_sync(STARTUP_DELAY).await;
            loop {
                let failed = me.refresh_all().await;
                let pause = if failed {
                    RETRY_INTERVAL
                } else {
                    REFRESH_INTERVAL
                };
                me.wait_for_sync(pause).await;
            }
        });
    }
}

async fn download_verified(
    url: &str,
    dest_dir: &Path,
    kind: &str,
    ext: &str,
    expected_hash: Option<&str>,
) -> Result<(PathBuf, String, u64), String> {
    let tmp_path = dest_dir.join(format!(
        ".{kind}-{}.tmp",
        chrono::Utc::now().timestamp_millis()
    ));

    let cleanup_tmp = |path: &Path| {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Could not remove wallpaper tmp file: {e}");
            }
        }
    };

    let result: Result<(PathBuf, String, u64), String> = async {
        let response = http::download_client()
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Request error: {e}"))?;
        if response.status().as_u16() != 200 {
            return Err(format!("HTTP {} for {url}", response.status().as_u16()));
        }
        if let Some(len) = response.content_length().filter(|len| *len > MAX_WALLPAPER_BYTES) {
            return Err(format!(
                "Refusing {url}: it declares {len} bytes, over the {MAX_WALLPAPER_BYTES}-byte limit"
            ));
        }

        let mut hasher = Sha256::new();
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        let mut stream = response.bytes_stream();
        let mut bytes = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("Stream error: {e}"))?;
            if bytes + chunk.len() as u64 > MAX_WALLPAPER_BYTES {
                return Err(format!(
                    "Refusing {url}: it exceeded the {MAX_WALLPAPER_BYTES}-byte limit"
                ));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Write error: {e}"))?;
            bytes += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        drop(file);

        let hash = hex::encode(hasher.finalize());
        if let Some(expected) = expected_hash {
            if hash != expected {
                return Err(format!("sha256 mismatch for {url} (got {}…)", &hash[..8]));
            }
        }

        let final_path = dest_dir.join(format!("{kind}-{}{ext}", &hash[..8]));
        if final_path.exists() {
            cleanup_tmp(&tmp_path);
        } else {
            std::fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())?;
        }
        Ok((final_path, hash, bytes))
    }
    .await;

    if result.is_err() {
        cleanup_tmp(&tmp_path);
    }
    result
}

pub(super) async fn get_wallpaper_media(app: &AppHandle) -> Result<Value, String> {
    let cache = app.state::<BackendState>().wallpaper.clone();
    if !cache.is_loaded() {
        cache.load_from_disk();
    }
    Ok(super::ok_with(json!({ "media": cache.media_map() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_video_extensions_count_as_animated() {
        assert!(is_video_ext(".mp4"));
        assert!(is_video_ext(".WEBM"));
        assert!(!is_video_ext(".webp"));
        assert!(!is_video_ext(".png"));
        assert!(!is_video_ext(".mkv"));
    }

    #[test]
    fn only_media_extensions_are_cached() {
        assert_eq!(
            wallpaper_ext("https://cdn.example.com/w/bg.WEBM", "wallpaper").as_deref(),
            Some(".webm")
        );
        assert_eq!(
            wallpaper_ext("https://cdn.example.com/w/still.png?v=2", "static").as_deref(),
            Some(".png")
        );
        for (url, kind, fallback) in [
            ("https://cdn.example.com/w/evil.html", "wallpaper", ".mp4"),
            ("https://cdn.example.com/w/evil.svg", "static", ".webp"),
            ("https://cdn.example.com/w/setup.exe", "wallpaper", ".mp4"),
            ("https://cdn.example.com/w/noext", "static", ".webp"),
        ] {
            assert_eq!(wallpaper_ext(url, kind).as_deref(), Some(fallback), "{url}");
        }
        assert_eq!(wallpaper_ext("file:///C:/x.mp4", "wallpaper"), None);
        assert_eq!(wallpaper_ext("not a url", "wallpaper"), None);
    }

    #[test]
    fn the_reuse_path_matches_the_download_naming() {
        let dir = Path::new("cache");
        let hash = "72f7d3da0123456789";
        assert_eq!(
            content_named_path(dir, "wallpaper", ".mp4", hash),
            Some(dir.join("wallpaper-72f7d3da.mp4"))
        );
        assert_eq!(content_named_path(dir, "static", ".webp", "abc"), None);
    }

    #[test]
    fn a_state_file_that_is_not_an_object_is_rejected() {
        for text in ["[]", "\"x\"", "0", "true", "null", "{"] {
            assert!(parse_state(text).is_err(), "accepted {text}");
        }
        let state = parse_state(r#"{"feedEtag":"abc"}"#).unwrap();
        assert_eq!(state["feedEtag"], "abc");
    }

    #[test]
    fn a_hidden_game_keeps_its_cache_through_the_grace_period() {
        let now = 1_700_000_000_000;
        assert_eq!(hidden_cache_action(&json!({}), now), HiddenCache::Mark);
        assert_eq!(hidden_cache_action(&json!({ "hiddenAt": "x" }), now), HiddenCache::Mark);
        assert_eq!(hidden_cache_action(&json!({ "hiddenAt": now }), now), HiddenCache::Keep);
        assert_eq!(
            hidden_cache_action(&json!({ "hiddenAt": now - HIDDEN_GRACE_MS + 1 }), now),
            HiddenCache::Keep
        );
        assert_eq!(
            hidden_cache_action(&json!({ "hiddenAt": now - HIDDEN_GRACE_MS }), now),
            HiddenCache::Remove
        );
        assert_eq!(hidden_cache_action(&json!({ "hiddenAt": now + 1 }), now), HiddenCache::Mark);
    }

    #[test]
    fn a_wake_left_behind_is_taken_without_waiting() {
        let notify = tokio::sync::Notify::new();
        notify.notify_one();
        assert!(notify.notified().now_or_never().is_some());
        assert!(notify.notified().now_or_never().is_none());
    }

    #[test]
    fn the_sync_order_follows_the_library_with_the_active_game_first() {
        let visible = json!(["zzz", "wuwa", "nope"]);
        assert_eq!(sync_order("zzz", &visible, true), vec!["zzz", "wuwa"]);
        assert_eq!(sync_order("gf1", &visible, true), vec!["gf1", "wuwa", "zzz"]);
        assert_eq!(sync_order("zzz", &visible, false), vec!["zzz"]);
        assert_eq!(sync_order("zzz", &json!([]), true), vec!["zzz"]);
    }
}
