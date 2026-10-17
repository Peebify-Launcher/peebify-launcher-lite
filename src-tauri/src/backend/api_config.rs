// ------------ API Config ------------
// Fetches the launcher's remote config (peebify.net/launcher/api.json), which holds the news and notice endpoints per game.
// It keeps a copy on disk for 12 hours, refreshes in the background, and falls back to that copy when you are offline.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

use super::http;

const CACHE_FILE_NAME: &str = "api-config-cache.json";
const CACHE_DURATION_MS: i64 = 1000 * 60 * 60 * 12;
const REFRESH_POLL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

pub const DEFAULT_API_CONFIG_URL: &str = "https://peebify.net/launcher/api.json";

pub fn api_config_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("PEEBIFY_API_CONFIG_URL") {
        if http::is_safe_override_url(&url) {
            return url;
        }
        log::warn!(
            "Ignoring PEEBIFY_API_CONFIG_URL: only https (or localhost) origins are accepted."
        );
    }
    DEFAULT_API_CONFIG_URL.to_string()
}

pub fn peebify_content_on(app: &tauri::AppHandle) -> bool {
    use tauri::Manager;
    app.try_state::<super::BackendState>()
        .is_none_or(|s| s.config.get("behavior.peebifyContent") != Value::Bool(false))
}

pub fn api_origin() -> String {
    url::Url::parse(&api_config_url())
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| "https://peebify.net".to_string())
}

#[derive(Default)]
struct Inner {
    config: Option<Value>,
    last_fetched: Option<i64>,
    etag: Option<String>,
}

pub struct ApiConfig {
    cache_file: PathBuf,
    inner: RwLock<Inner>,
    save_lock: Mutex<()>,
}

impl ApiConfig {
    pub fn new(user_data: &Path) -> Arc<Self> {
        Arc::new(Self {
            cache_file: user_data.join(CACHE_FILE_NAME),
            inner: RwLock::new(Inner::default()),
            save_lock: Mutex::new(()),
        })
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.read().config.is_some()
    }

    pub async fn initialize(self: &Arc<Self>) -> Result<(), String> {
        self.load_from_cache();

        if !self.should_refresh() {
            log::info!(
                "API config: using the cache ({}), refreshing in the background",
                self.cache_age_label()
            );
            self.refresh_in_background();
            return Ok(());
        }

        match self.fetch_and_cache().await {
            Ok(()) => Ok(()),
            Err(e) if self.is_loaded() => {
                log::warn!(
                    "API config refresh failed, serving the cache ({}): {e}",
                    self.cache_age_label()
                );
                Ok(())
            }
            Err(e) => Err(format!(
                "no API config from {} and nothing cached: {e}",
                api_config_url()
            )),
        }
    }

    pub fn start_periodic_refresh(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(REFRESH_POLL).await;
                if !http::is_online_cached() || !me.should_refresh() {
                    continue;
                }
                if let Err(e) = me.fetch_and_cache().await {
                    log::warn!("Periodic API config refresh failed: {e}");
                }
            }
        });
    }

    fn cache_age_label(&self) -> String {
        match self.inner.read().last_fetched {
            Some(last) => format_age(chrono::Utc::now().timestamp_millis() - last),
            None => "age unknown".to_string(),
        }
    }

    fn load_from_cache(&self) {
        match std::fs::read_to_string(&self.cache_file) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(cached) => {
                    let config = cached.get("config").cloned();
                    let timestamp = cached.get("timestamp").and_then(|v| v.as_i64());
                    let etag = cached
                        .get("etag")
                        .and_then(Value::as_str)
                        .filter(|e| !e.is_empty())
                        .map(str::to_string);
                    if let (Some(config), Some(timestamp)) =
                        (config.filter(|c| !c.is_null()), timestamp)
                    {
                        if !validate_config(&config) {
                            log::warn!("Ignoring an API config cache with an invalid structure");
                            return;
                        }
                        let mut inner = self.inner.write();
                        inner.config = Some(config);
                        inner.last_fetched = Some(timestamp);
                        inner.etag = etag;
                        log::debug!("Loaded API config from cache");
                    }
                }
                Err(e) => log::warn!("Failed to load API config cache: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Failed to load API config cache: {e}"),
        }
    }

    fn save_to_cache(&self) {
        let _guard = self.save_lock.lock();
        let payload = {
            let inner = self.inner.read();
            json!({ "config": inner.config, "timestamp": inner.last_fetched, "etag": inner.etag })
        };
        let write = || -> Result<(), String> {
            let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
            super::fs_util::write_atomic(&self.cache_file, text.as_bytes())
        };
        match write() {
            Ok(()) => log::debug!("Saved API config to cache"),
            Err(e) => log::warn!("Failed to save API config cache: {e}"),
        }
    }

    fn should_refresh(&self) -> bool {
        let inner = self.inner.read();
        let (Some(_), Some(last)) = (&inner.config, inner.last_fetched) else {
            return true;
        };
        cache_expired(last, chrono::Utc::now().timestamp_millis())
    }

    async fn fetch_and_cache(&self) -> Result<(), String> {
        let url = api_config_url();
        let known = self.inner.read().etag.clone();
        log::debug!("Fetching API config from {url}");

        let fetched = http::with_retry(
            || http::get_text_conditional(&url, known.as_deref()),
            3,
            1000,
            "API config fetch",
        )
        .await?;

        let (text, etag) = match fetched {
            http::Conditional::Unchanged => {
                self.inner.write().last_fetched = Some(chrono::Utc::now().timestamp_millis());
                self.save_to_cache();
                log::info!("API config unchanged (304), keeping the cached copy");
                return Ok(());
            }
            http::Conditional::Fresh { text, etag } => (text, etag),
        };

        let config: Value =
            serde_json::from_str(&text).map_err(|e| format!("Invalid API config JSON: {e}"))?;
        if !validate_config(&config) {
            return Err("Invalid API config structure from server".to_string());
        }

        {
            let mut inner = self.inner.write();
            inner.config = Some(config);
            inner.last_fetched = Some(chrono::Utc::now().timestamp_millis());
            inner.etag = etag;
        }
        self.save_to_cache();
        log::info!("API config fetched and cached successfully");
        Ok(())
    }

    fn refresh_in_background(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if let Err(e) = me.fetch_and_cache().await {
                log::warn!("Background API config refresh failed: {e}");
            }
        });
    }

    fn client_section(&self, client_key: Option<&str>) -> Option<Value> {
        let inner = self.inner.read();
        let clients = inner.config.as_ref()?.get("clients")?;
        let key = client_key
            .filter(|k| !k.is_empty())
            .unwrap_or(super::game_profiles::DEFAULT_GAME_ID);
        clients.get(key).cloned()
    }

    fn url_for_client(&self, client_key: Option<&str>, section: &str) -> Option<String> {
        self.client_section(client_key)?
            .get(section)?
            .get("url")?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn news_url_for_client(&self, client_key: Option<&str>) -> Option<String> {
        self.url_for_client(client_key, "news-notices")
    }

    pub fn wallpapers_slogan_url_for_client(&self, client_key: Option<&str>) -> Option<String> {
        self.url_for_client(client_key, "wallpapers-slogan")
    }
}

fn validate_config(config: &Value) -> bool {
    let Some(clients) = config.get("clients").filter(|c| c.is_object()) else {
        log::error!("Missing clients in API config");
        return false;
    };
    let Some(wuwa) = clients.get("wuwa") else {
        log::error!("Missing clients.wuwa in API config");
        return false;
    };
    let has_news_url = wuwa
        .get("news-notices")
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .is_some_and(|u| !u.is_empty());
    if !has_news_url {
        log::error!("Missing news-notices.url for WUWA in API config");
        return false;
    }
    log::debug!("API config structure validated successfully");
    true
}

fn cache_expired(last: i64, now: i64) -> bool {
    last > now || now - last > CACHE_DURATION_MS
}

fn format_age(age_ms: i64) -> String {
    let minutes = age_ms.max(0) / 60_000;
    if minutes < 60 {
        format!("age {minutes}m")
    } else {
        format!("age {}h", minutes / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_config_needs_only_news() {
        let config = json!({
            "clients": { "wuwa": { "news-notices": { "url": "https://example.com/news.json" } } }
        });
        assert!(validate_config(&config));
        assert!(!validate_config(&json!({ "clients": { "wuwa": {} } })));
    }

    fn cached_api(name: &str, client: &str, age_ms: i64) -> (PathBuf, Arc<ApiConfig>) {
        let dir = std::env::temp_dir().join(format!("peebify-api-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = json!({ "clients": { client: { "news-notices": { "url": "https://example.com/news.json" } } } });
        let stamp = chrono::Utc::now().timestamp_millis() - age_ms;
        std::fs::write(
            dir.join(CACHE_FILE_NAME),
            json!({ "config": config, "timestamp": stamp, "etag": "abc" }).to_string(),
        )
        .unwrap();
        let api = ApiConfig::new(&dir);
        api.load_from_cache();
        (dir, api)
    }

    #[test]
    fn load_from_cache_reads_a_stale_file_and_skips_an_invalid_one() {
        let (dir, api) = cached_api("stale", "wuwa", CACHE_DURATION_MS * 2);
        assert!(api.is_loaded());
        assert!(api.should_refresh());
        let news = Some("https://example.com/news.json");
        assert_eq!(api.news_url_for_client(Some("wuwa")).as_deref(), news);
        assert_eq!(api.news_url_for_client(None).as_deref(), news);
        let _ = std::fs::remove_dir_all(&dir);

        let (dir, api) = cached_api("invalid", "osLive", 0);
        assert!(!api.is_loaded());
        assert!(api.should_refresh());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
