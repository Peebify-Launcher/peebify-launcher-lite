// ------------ Yostar Launcher API ------------
// Talks to Yostar's PC launcher service for Arknights: signs each request, resolves the latest build and its CDNs,
// and turns the loose-file manifest into download resources checked with CRC-64. Also maps the launcher's news feed
// and sizes the install preview.
use std::path::Path;

use futures::future::BoxFuture;
use serde_json::{json, Value};

use super::engine::{combine_url, resource_list_preview, Preview};
use super::fs_util::md5_hex;
use super::game_profiles::{self, profile_str, required_str};
use super::http;
use super::manifest::{json_u64, Cx, Entry, Hash, Manifest, Policy, Publisher};
use super::news::{article, envelope, slide, MAX_ITEMS, MAX_SLIDES};

const OK_CODE: i64 = 200;
const LATEST_LABEL: &str = "latest";
const API_TEXT_LIMIT: u64 = 4 << 20;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

pub fn sign_header(game_tag: &str, launcher_version: &str, sign_key: &str, time: u64, body: &str) -> String {
    let head = format!(
        "{{\"game_tag\":{},\"time\":{time},\"version\":{}}}",
        quoted(game_tag),
        quoted(launcher_version)
    );
    let sign = md5_hex(format!("{head}{body}{sign_key}").as_bytes());
    format!("{{\"head\":{head},\"sign\":\"{sign}\"}}")
}

async fn signed_get_once(profile: &Value, path_and_query: &str) -> Result<Value, String> {
    let api = required_str(profile, "ysApiUrl")?;
    let header = sign_header(
        required_str(profile, "ysGameTag")?,
        required_str(profile, "ysLauncherVersion")?,
        required_str(profile, "ysSignKey")?,
        unix_now(),
        "",
    );
    let url = combine_url(api, path_and_query);
    let response = http::client()
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, header)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("Request error: {}", http::describe(&e)))?;
    let status = response.status().as_u16();
    http::note_reachable();
    if status != 200 {
        return Err(format!("HTTP {status} for {url}"));
    }
    let text = http::read_text_capped(response, &url, API_TEXT_LIMIT).await?;
    let body: Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid JSON from {url}: {e}"))?;
    unwrap_envelope(body, path_and_query)
}

fn unwrap_envelope(body: Value, what: &str) -> Result<Value, String> {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != OK_CODE {
        let message = body
            .get("msg")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Err(format!("Yostar API {what} answered code {code}: {message}"));
    }
    Ok(body.get("data").cloned().unwrap_or(Value::Null))
}

async fn signed_get(profile: &Value, path_and_query: &str) -> Result<Value, String> {
    http::with_retry(
        || signed_get_once(profile, path_and_query),
        3,
        1000,
        "Yostar launcher API",
    )
    .await
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub basis: String,
}

pub fn release_from(data: &Value) -> Result<Release, String> {
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let release = Release {
        version: text("game_latest_version"),
        basis: text("game_latest_file_path"),
    };
    if release.version.is_empty() || release.basis.is_empty() {
        return Err("The Yostar API did not name a current game build.".to_string());
    }
    Ok(release)
}

pub async fn fetch_release(profile: &Value) -> Result<Release, String> {
    release_from(&signed_get(profile, "/api/launcher/game/config").await?)
}

pub fn cdns_from(data: &Value) -> Vec<String> {
    let mut cdns: Vec<String> = Vec::new();
    for key in ["primary_cdn", "back_up_cdn"] {
        let Some(url) = data.get(key).and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if url.starts_with("https://") && !cdns.iter().any(|c| c == url) {
            cdns.push(url.to_string());
        }
    }
    cdns
}

pub(super) async fn fetch_cdns(profile: &Value) -> Result<Vec<String>, String> {
    let cdns = cdns_from(&signed_get(profile, "/api/launcher/advanced/game/download/cdn").await?);
    if cdns.is_empty() {
        return Err("The Yostar API returned no download servers.".to_string());
    }
    Ok(cdns)
}

fn query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn encode_path(path: &str) -> String {
    path.split('/').map(query_value).collect::<Vec<_>>().join("/")
}

async fn fetch_manifest(profile: &Value, release: &Release) -> Result<Value, String> {
    let path = format!(
        "/api/launcher/game/config/json?version={}&file_path={}",
        query_value(&release.version),
        query_value(&release.basis)
    );
    let data = signed_get(profile, &path).await?;
    let url = data.get("url").and_then(Value::as_str).unwrap_or("").trim();
    if !url.starts_with("https://") {
        return Err("The Yostar API did not return a file list for this build.".to_string());
    }
    http::with_retry(|| http::get_json(url), 3, 1000, "Yostar file list").await
}

pub fn manifest_entries(manifest: &Value, base: &str) -> Vec<Entry> {
    let empty = Vec::new();
    manifest
        .get("file")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter_map(|entry| {
            let dest = entry.get("path")?.as_str()?.trim().trim_start_matches('/');
            if dest.is_empty() || dest.split('/').any(|part| part == "..") {
                return None;
            }
            let size = json_u64(entry.get("size"))?;
            let hash = json_u64(entry.get("hash"))?;
            Some(Entry::new(dest, size, Hash::Crc64(hash), combine_url(base, &encode_path(dest))))
        })
        .collect()
}

pub fn manifest_source(manifest: &Value) -> &str {
    manifest.get("source").and_then(Value::as_str).unwrap_or("").trim()
}

async fn resolve(profile: &Value) -> Result<Manifest, String> {
    let name = game_profiles::display_name(profile);
    let (release, cdns) = tokio::try_join!(fetch_release(profile), fetch_cdns(profile))?;
    let manifest = fetch_manifest(profile, &release).await?;
    let source = manifest_source(&manifest);
    let mirrors: Vec<String> = cdns.iter().map(|cdn| combine_url(cdn, source)).collect();
    let entries = manifest_entries(&manifest, &mirrors[0]);
    if entries.is_empty() {
        return Err(format!("{name}: the server returned an empty file list."));
    }
    log::info!(
        "{name} build {} resolved with {} files, {:.2} GB.",
        release.version,
        entries.len(),
        entries.iter().map(|e| e.size).sum::<u64>() as f64 / 1_073_741_824.0
    );
    Ok(Manifest { version: release.version, entries, mirrors, ..Default::default() })
}

// ------------ Publisher ------------
// Arknights' file list. A full repair records the build it repaired against.
pub(super) struct Yostar;

impl Publisher for Yostar {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(resolve(cx.profile))
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({ "version": fetch_release(profile).await?.version })) })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn policy(&self, _dir: &Path) -> Policy {
        Policy { records: true, ..Policy::default() }
    }
}

fn local_date_from_ms(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(at) if ms > 0 => at.format("%m-%d").to_string(),
        _ => String::new(),
    }
}

fn collect_rows(groups: &[Value], latest: bool) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::new();
    let mut rows: Vec<(i64, Value)> = Vec::new();
    for group in groups {
        let label = group.get("typeLabel").and_then(Value::as_str).unwrap_or("");
        if label.eq_ignore_ascii_case(LATEST_LABEL) != latest {
            continue;
        }
        for row in group.get("rows").and_then(Value::as_array).into_iter().flatten() {
            let title = row.get("title").and_then(Value::as_str).unwrap_or("").trim();
            let link = row.get("link").and_then(Value::as_str).unwrap_or("").trim();
            if title.is_empty() || seen.iter().any(|s| s == link) {
                continue;
            }
            seen.push(link.to_string());
            let at = super::news::as_i64(row.get("publishTime"));
            rows.push((at, article(title, link, local_date_from_ms(at))));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, row)| row).take(MAX_ITEMS).collect()
}

fn collect_notices(list: &[Value]) -> Vec<Value> {
    list.iter()
        .flat_map(|group| {
            group
                .get("notice_detail_list")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|item| {
            let title = item.get("notice_title").and_then(Value::as_str)?.trim();
            if title.is_empty() {
                return None;
            }
            let link = item.get("jump_url").and_then(Value::as_str).unwrap_or("");
            let time = item.get("notice_time").and_then(Value::as_str).unwrap_or("");
            Some(article(title, link, super::news::short_date(time)))
        })
        .take(MAX_ITEMS)
        .collect()
}

pub fn news_from(data: &Value, site: &str) -> Value {
    let slides = slides_from(data, site);

    let feed = &data["news_list"];
    if feed.get("code").and_then(Value::as_i64) == Some(0) {
        let groups = feed["data"]["news"].as_array().cloned().unwrap_or_default();
        return envelope(collect_rows(&groups, false), collect_rows(&groups, true), slides);
    }
    let notices = data
        .get("notice_list")
        .and_then(Value::as_array)
        .map(|list| collect_notices(list))
        .unwrap_or_default();
    envelope(notices, Vec::new(), slides)
}

fn slides_from(data: &Value, site: &str) -> Vec<Value> {
    data.get("operations_banner_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|banner| {
            let image = banner.get("banner_img").and_then(Value::as_str)?.trim();
            if image.is_empty() {
                return None;
            }
            let jump = banner.get("jump_url").and_then(Value::as_str).unwrap_or("").trim();
            Some(slide(image, if jump.is_empty() { site } else { jump }))
        })
        .take(MAX_SLIDES)
        .collect()
}

// ------------ Install Preview ------------
// The download and install size of the file list.
pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    Ok(resource_list_preview(resolve(profile).await?))
}

// ------------ Website News ------------
// The arknights.global news page is a single-page app over a plain JSON API: one call lists the categories, one call lists the posts in a category.
// Its rows already look like the launcher feed's rows (title, link, publishTime), so they are wrapped into the same groups and share collect_rows.
const SITE_FALLBACK_TYPES: [&str; 3] = ["latest", "event", "contest"];

fn site_data(body: Value, what: &str) -> Result<Value, String> {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let message = body.get("message").and_then(Value::as_str).unwrap_or("no message");
        return Err(format!("News site {what} answered code {code}: {message}"));
    }
    Ok(body.get("data").cloned().unwrap_or(Value::Null))
}

fn site_types(data: &Value) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for tab in data.as_array().into_iter().flatten() {
        let value = tab.get("value").and_then(Value::as_str).unwrap_or("").trim();
        let plain = !value.is_empty()
            && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if plain && !types.iter().any(|t| t == value) {
            types.push(value.to_string());
        }
    }
    if types.is_empty() {
        types = SITE_FALLBACK_TYPES.iter().map(|t| t.to_string()).collect();
    }
    types
}

fn site_group(kind: &str, data: &Value, site: &str) -> Value {
    let rows: Vec<Value> = data
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            let link = row.get("link").and_then(Value::as_str).unwrap_or("").trim();
            let id = row.get("id").and_then(Value::as_i64);
            let link = match (link.is_empty(), id) {
                (true, Some(id)) => format!("{}/{id}", site.trim_end_matches('/')),
                _ => link.to_string(),
            };
            serde_json::json!({
                "title": row.get("title").cloned().unwrap_or(Value::Null),
                "link": link,
                "publishTime": row.get("publishTime").cloned().unwrap_or(Value::Null),
                "bigImage": row.get("bigImage").cloned().unwrap_or(Value::Null),
                "smallImage": row.get("smallImage").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    serde_json::json!({ "typeLabel": kind, "rows": rows })
}

fn site_slides(groups: &[Value]) -> Vec<Value> {
    let mut rows: Vec<(i64, &Value)> = groups
        .iter()
        .filter(|group| {
            group
                .get("typeLabel")
                .and_then(Value::as_str)
                .is_some_and(|label| label.eq_ignore_ascii_case(LATEST_LABEL))
        })
        .flat_map(|group| group.get("rows").and_then(Value::as_array).into_iter().flatten())
        .map(|row| (super::news::as_i64(row.get("publishTime")), row))
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));

    let mut seen: Vec<&str> = Vec::new();
    let mut slides = Vec::new();
    for (_, row) in rows {
        let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("").trim();
        let image = [text("bigImage"), text("smallImage")]
            .into_iter()
            .find(|url| url.starts_with("https://"));
        let Some(image) = image else { continue };
        if seen.contains(&image) {
            continue;
        }
        seen.push(image);
        slides.push(slide(image, text("link")));
        if slides.len() == MAX_SLIDES {
            break;
        }
    }
    slides
}

async fn site_get(url: &str) -> Result<Value, String> {
    site_data(http::get_json(url).await?, url)
}

async fn get_site_groups(api: &str, site: &str) -> Result<Vec<Value>, String> {
    let api = api.trim_end_matches('/');
    let types = match site_get(&format!("{api}/type")).await {
        Ok(data) => site_types(&data),
        Err(e) => {
            log::warn!("news: arknights.global category list failed ({e}); using the built-in categories");
            site_types(&Value::Null)
        }
    };
    let requests = types.iter().map(|kind| async move {
        let url = format!("{api}/list?index=1&size={MAX_ITEMS}&type={kind}");
        (kind.as_str(), site_get(&url).await)
    });
    let mut groups = Vec::new();
    let mut last_error = String::new();
    for (kind, result) in futures::future::join_all(requests).await {
        match result {
            Ok(data) => groups.push(site_group(kind, &data, site)),
            Err(e) => last_error = e,
        }
    }
    if groups.is_empty() {
        return Err(last_error);
    }
    Ok(groups)
}

pub async fn get_news(profile: &Value) -> Result<Value, String> {
    let site = profile_str(profile, "ysNewsUrl");
    let api = profile_str(profile, "ysNewsApi");
    let (launcher, website) = futures::join!(
        signed_get(profile, "/api/launcher/operations/resource"),
        async {
            match api {
                "" => None,
                api => Some(get_site_groups(api, site).await),
            }
        }
    );
    match (website, launcher) {
        (Some(Ok(groups)), launcher) => {
            let mut slides = site_slides(&groups);
            if let Ok(data) = launcher {
                for extra in slides_from(&data, site) {
                    let image = extra.get("url").cloned();
                    if slides.len() < MAX_SLIDES && !slides.iter().any(|s| s.get("url") == image.as_ref()) {
                        slides.push(extra);
                    }
                }
            }
            Ok(envelope(collect_rows(&groups, false), collect_rows(&groups, true), slides))
        }
        (website, Ok(data)) => {
            if let Some(Err(e)) = website {
                log::warn!("news: arknights.global failed ({e}); using the launcher feed");
            }
            Ok(news_from(&data, site))
        }
        (_, Err(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_signature_covers_the_head_exactly_as_sent() {
        let header = sign_header("Arknights_EN", "1.8.1", "salt", 1700000000, "");
        let head = r#"{"game_tag":"Arknights_EN","time":1700000000,"version":"1.8.1"}"#;
        let expected = md5_hex(format!("{head}salt").as_bytes());
        assert_eq!(header, format!(r#"{{"head":{head},"sign":"{expected}"}}"#));
        let parsed: Value = serde_json::from_str(&header).unwrap();
        assert_eq!(parsed["head"]["game_tag"], "Arknights_EN");
    }

    #[test]
    fn a_non_200_envelope_is_an_error() {
        let failed = unwrap_envelope(json!({ "code": 401, "msg": "sign error" }), "/x");
        assert!(failed.unwrap_err().contains("sign error"));
        let ok = unwrap_envelope(json!({ "code": 200, "data": { "a": 1 } }), "/x").unwrap();
        assert_eq!(ok["a"], 1);
    }

    #[test]
    fn a_release_needs_both_version_and_file_path() {
        let data = json!({
            "game_latest_version": "041.2.0",
            "game_latest_file_path": "prod/ZIP_TEMP/Arknights_EN_TEMP/Arknights_EN-041.2.0-game.zip",
        });
        assert_eq!(release_from(&data).unwrap().version, "041.2.0");
        assert!(release_from(&json!({ "game_latest_version": "041.2.0" })).is_err());
    }

    #[test]
    fn the_primary_cdn_comes_before_the_backup() {
        let data = json!({
            "back_up_cdn": "https://launcher-pkg-ark-en-bk.yo-star.com",
            "primary_cdn": "https://launcher-pkg-ark-en.yo-star.com",
        });
        assert_eq!(
            cdns_from(&data),
            vec![
                "https://launcher-pkg-ark-en.yo-star.com".to_string(),
                "https://launcher-pkg-ark-en-bk.yo-star.com".to_string(),
            ]
        );
        assert!(cdns_from(&json!({ "primary_cdn": "http://insecure.example" })).is_empty());
    }

    #[test]
    fn manifest_entries_become_crc64_resources() {
        let manifest = json!({
            "source": "/Arknights_EN-041.2.0-game",
            "file": [
                { "path": "/Arknights.exe", "hash": "1157254865370011820", "size": "675304" },
                { "path": "/Arknights_Data/Plugins/x86_64/TQM64/dump/space.txt", "hash": "0", "size": "0" },
                { "path": "/../escape.dll", "hash": "1", "size": "1" },
                { "path": "/broken.bin", "hash": "nope", "size": "1" },
            ],
        });
        let entries = manifest_entries(&manifest, "https://cdn.example/game");
        assert_eq!(entries.len(), 2);
        assert_eq!(&*entries[0].path, "Arknights.exe");
        assert_eq!(entries[0].size, 675304);
        assert_eq!(entries[0].hash.expected(), "crc64:1157254865370011820");
        assert_eq!(entries[1].hash.expected(), "crc64:0");
        assert_eq!(entries[1].url(), "https://cdn.example/game/Arknights_Data/Plugins/x86_64/TQM64/dump/space.txt");
        assert!(!entries[0].live);
        assert_eq!(manifest_source(&manifest), "/Arknights_EN-041.2.0-game");
    }

    #[test]
    fn paths_with_hashes_and_spaces_are_escaped_per_segment() {
        assert_eq!(
            encode_path("Arknights_Data/refs/char_103_angel_sale#8.ab"),
            "Arknights_Data/refs/char_103_angel_sale%238.ab"
        );
        assert_eq!(
            encode_path("Arknights_Data/Resources/unity default resources"),
            "Arknights_Data/Resources/unity%20default%20resources"
        );
        assert_eq!(encode_path("a?b/100%.txt"), "a%3Fb/100%25.txt");
    }

    #[test]
    fn the_news_feed_splits_latest_from_events_and_keeps_banners() {
        let data = json!({
            "operations_banner_list": [
                { "banner_img": "https://cdn.example/banner.png", "jump_url": "" },
                { "banner_img": "", "jump_url": "https://ignored.example" },
            ],
            "news_list": {
                "code": 0,
                "data": { "news": [
                    { "typeLabel": "LATEST", "rows": [
                        { "link": "https://arknights.global/news/1", "publishTime": 1000, "title": "Older" },
                        { "link": "https://arknights.global/news/2", "publishTime": 2000, "title": "Newer" },
                    ]},
                    { "typeLabel": "event", "rows": [
                        { "link": "https://arknights.global/news/2", "publishTime": 2000, "title": "Newer" },
                    ]},
                    { "typeLabel": "contest", "rows": [
                        { "link": "https://arknights.global/news/3", "publishTime": 3000, "title": "Contest" },
                    ]},
                ]},
            },
        });
        let news = news_from(&data, "https://www.arknights.global/");
        let latest = news["guidance"]["news"]["contents"].as_array().unwrap();
        let notices = news["guidance"]["notice"]["contents"].as_array().unwrap();
        assert_eq!(latest[0]["content"], "Newer");
        assert_eq!(latest.len(), 2);
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0]["content"], "Contest");
        let slides = news["slideshow"].as_array().unwrap();
        assert_eq!(slides.len(), 1);
        assert_eq!(slides[0]["jumpUrl"], "https://www.arknights.global/");
    }

    #[test]
    fn website_posts_become_the_same_feed_as_the_launcher_rows() {
        let latest = json!({ "count": 3, "rows": [
            { "id": 11, "title": "Duel Channel", "link": "https://arknights.global/news/11", "publishTime": 2000, "type": "event", "content": "<p>x</p>" },
            { "id": 10, "title": "Patch Notes", "link": "", "publishTime": 1000, "type": "news" },
            { "id": 9, "title": "   ", "link": "https://arknights.global/news/9", "publishTime": 500 },
        ]});
        let contest = json!({ "count": 1, "rows": [
            { "id": 7, "title": "Video Contest", "link": "https://arknights.global/news/7", "publishTime": 3000 },
        ]});
        let groups = vec![
            site_group("latest", &latest, "https://www.arknights.global/news"),
            site_group("contest", &contest, ""),
        ];
        let news = collect_rows(&groups, true);
        let notices = collect_rows(&groups, false);
        assert_eq!(news.len(), 2);
        assert_eq!(news[0]["content"], "Duel Channel");
        assert_eq!(news[1]["jumpUrl"], "https://www.arknights.global/news/10");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["content"], "Video Contest");
    }

    #[test]
    fn website_banners_make_the_slideshow_newest_first_and_open_their_posts() {
        let latest = json!({ "rows": [
            { "id": 1, "title": "Old", "link": "https://arknights.global/news/1", "publishTime": 1000, "bigImage": "https://cdn.example/old.png", "smallImage": "https://cdn.example/old-s.png" },
            { "id": 2, "title": "New", "link": "https://arknights.global/news/2", "publishTime": 3000, "bigImage": "https://cdn.example/new.png", "smallImage": "" },
            { "id": 3, "title": "Small only", "link": "https://arknights.global/news/3", "publishTime": 2000, "bigImage": "", "smallImage": "https://cdn.example/mid-s.png" },
            { "id": 4, "title": "No art", "link": "https://arknights.global/news/4", "publishTime": 4000, "bigImage": "", "smallImage": "" },
            { "id": 5, "title": "Insecure", "link": "https://arknights.global/news/5", "publishTime": 5000, "bigImage": "http://cdn.example/x.png" },
            { "id": 6, "title": "Repeat", "link": "https://arknights.global/news/6", "publishTime": 500, "bigImage": "https://cdn.example/new.png" },
        ]});
        let contest = json!({ "rows": [
            { "id": 7, "title": "Contest", "link": "https://arknights.global/news/7", "publishTime": 9000, "bigImage": "https://cdn.example/contest.png" },
        ]});
        let groups = vec![site_group("latest", &latest, ""), site_group("contest", &contest, "")];
        let slides = site_slides(&groups);
        let images: Vec<&str> = slides.iter().filter_map(|s| s["url"].as_str()).collect();
        assert_eq!(
            images,
            vec!["https://cdn.example/new.png", "https://cdn.example/mid-s.png", "https://cdn.example/old.png"]
        );
        assert_eq!(slides[0]["jumpUrl"], "https://arknights.global/news/2");
    }

    #[test]
    fn website_categories_are_validated_and_fall_back_to_the_known_three() {
        let data = json!([
            { "label": "LATEST", "value": "latest" },
            { "label": "event", "value": "event" },
            { "label": "dup", "value": "event" },
            { "label": "bad", "value": "a&b=c" },
        ]);
        assert_eq!(site_types(&data), vec!["latest".to_string(), "event".to_string()]);
        assert_eq!(site_types(&Value::Null).len(), 3);
    }

    #[test]
    fn a_website_error_code_is_an_error() {
        let failed = site_data(json!({ "code": 500, "message": "boom" }), "/x");
        assert!(failed.unwrap_err().contains("boom"));
        let ok = site_data(json!({ "code": 0, "data": { "rows": [] } }), "/x").unwrap();
        assert!(ok["rows"].is_array());
    }

    #[test]
    fn the_older_notice_list_shape_still_fills_notices() {
        let data = json!({
            "news_list": { "code": 1 },
            "notice_list": [{
                "notice_type": "update",
                "notice_detail_list": [
                    { "notice_title": "Maintenance", "jump_url": "https://x.example", "notice_time": "2026-09-30" },
                ],
            }],
        });
        let news = news_from(&data, "");
        assert_eq!(news["guidance"]["notice"]["contents"][0]["content"], "Maintenance");
        assert_eq!(news["guidance"]["notice"]["contents"][0]["time"], "09-30");
    }
}
