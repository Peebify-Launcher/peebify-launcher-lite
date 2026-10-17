// ------------ Arknights: Endfield ------------
// Everything Hypergryph: the Gryphline launcher API that names the latest Endfield version, its download packs and
// the launcher news, the manifest of the game those split zip packs hold, and the install preview. A first install
// downloads the packs and unpacks them; an update, repair and verify read just the files they need out of the packs.

use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{json, Value};

use super::engine::{archive_space, JobKind, Preview};
use super::fs_util::same_version;
use super::game_manager::local_game_version_at;
use super::manifest::{origin_of, Container, Cx, Entry, Hash, Manifest, Pack, Policy, Publisher};
use super::validator::{self, Record, Triage};
use super::game_profiles::{self, required_str};
use super::{http, members, news};

// ------------ Endfield Feed ------------
// Finds the latest Endfield version and its download packs, and pulls the launcher news. The packs it returns are
// the split zip files the manifest below is built from.

struct HgApiConfig {
    api_url: String,
    web_api_url: String,
    appcode: String,
    channel: String,
    sub_channel: String,
    seq: String,
    language: String,
}

fn api_config(profile: &Value) -> Result<HgApiConfig, String> {
    let field = |key: &str| required_str(profile, key).map(str::to_string);
    Ok(HgApiConfig {
        api_url: field("hgApiUrl")?,
        web_api_url: field("hgWebApiUrl")?,
        appcode: field("hgAppCode")?,
        channel: field("hgChannel")?,
        sub_channel: field("hgSubChannel")?,
        seq: field("hgSeq")?,
        language: field("hgLanguage")?,
    })
}

async fn post_batch_proxy_once(url: &str, body: &Value) -> Result<Value, String> {
    let response = http::client()
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Request error: {e}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status} for {url}"));
    }
    let bytes = http::read_capped(response, url).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid JSON from {url}: {e}"))
}

async fn post_batch_proxy(url: &str, body: &Value) -> Result<Value, String> {
    http::with_retry(
        || post_batch_proxy_once(url, body),
        3,
        1000,
        "Gryphline batch_proxy",
    )
    .await
}

fn find_rsp<'a>(response: &'a Value, kind: &str) -> Option<&'a Value> {
    response["proxy_rsps"]
        .as_array()?
        .iter()
        .find(|r| r["kind"].as_str() == Some(kind))
}

fn response_snippet(response: &Value) -> String {
    response.to_string().chars().take(300).collect()
}

fn pack_size(pack: &Value) -> u64 {
    match pack.get("package_size") {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn pack_str<'a>(pack: &'a Value, key: &str) -> &'a str {
    pack.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn validate_packs(packs: &[Value]) -> Result<(), String> {
    for (index, pack) in packs.iter().enumerate() {
        if pack_str(pack, "url").trim().is_empty() {
            return Err(format!("get_latest_game_rsp pack {index} has no url"));
        }
        if pack_str(pack, "md5").trim().is_empty() {
            return Err(format!("get_latest_game_rsp pack {index} has no md5"));
        }
        if pack_size(pack) == 0 {
            return Err(format!(
                "get_latest_game_rsp pack {index} has no valid package_size"
            ));
        }
    }
    Ok(())
}

fn latest_game_from_response(response: &Value) -> Result<Value, String> {
    let rsp = find_rsp(response, "get_latest_game")
        .and_then(|r| r.get("get_latest_game_rsp"))
        .ok_or_else(|| {
            format!(
                "batch_proxy response has no get_latest_game_rsp (response: {})",
                response_snippet(response)
            )
        })?;

    let version = rsp
        .get("version")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "get_latest_game_rsp has no version (response: {})",
                response_snippet(response)
            )
        })?;
    let packs = rsp["pkg"]["packs"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            format!(
                "get_latest_game_rsp has no pkg.packs (response: {})",
                response_snippet(response)
            )
        })?;
    validate_packs(packs)?;

    Ok(json!({ "version": version, "packs": packs }))
}

pub async fn get_latest_game(profile: &Value) -> Result<Value, String> {
    let cfg = api_config(profile)?;
    let body = json!({
        "seq": cfg.seq,
        "proxy_reqs": [{
            "kind": "get_latest_game",
            "get_latest_game_req": {
                "appcode": cfg.appcode,
                "launcher_appcode": cfg.appcode,
                "channel": cfg.channel,
                "sub_channel": cfg.sub_channel,
                "version": "",
            },
        }],
    });
    let response = post_batch_proxy(&cfg.api_url, &body).await?;
    latest_game_from_response(&response)
}

pub fn packs_as_entries(packs: &Value) -> Vec<Entry> {
    let empty: Vec<Value> = Vec::new();
    packs
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|pack| {
            let url = pack_str(pack, "url");
            let dest = std::path::Path::new(url)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            Entry::new(dest, pack_size(pack), Hash::parse(pack_str(pack, "md5")), url)
        })
        .collect()
}

fn format_notice_date(start_ts: &Value) -> String {
    news::int_of(Some(start_ts)).map(news::local_month_day).unwrap_or_default()
}

fn news_from_response(response: &Value) -> Value {
    let empty: Vec<Value> = Vec::new();
    let banner_rsp = find_rsp(response, "get_banner");
    let announcement_rsp = find_rsp(response, "get_announcement");
    if banner_rsp.is_none() || announcement_rsp.is_none() {
        log::warn!(
            "Gryphline news response is missing get_banner or get_announcement (response: {})",
            response_snippet(response)
        );
    }

    let slideshow: Vec<Value> = banner_rsp
        .and_then(|r| r["get_banner_rsp"]["banners"].as_array())
        .unwrap_or(&empty)
        .iter()
        .filter_map(|banner| {
            let url = pack_str(banner, "url");
            if url.is_empty() {
                return None;
            }
            Some(json!({
                "url": url,
                "jumpUrl": pack_str(banner, "jump_url"),
            }))
        })
        .take(news::MAX_SLIDES)
        .collect();

    let mut notice: Vec<Value> = Vec::new();
    let mut news_items: Vec<Value> = Vec::new();
    let tabs = announcement_rsp
        .and_then(|r| r["get_announcement_rsp"]["tabs"].as_array())
        .unwrap_or(&empty);
    for tab in tabs {
        let tab_name = pack_str(tab, "tabName").to_lowercase();
        let items = tab["announcements"].as_array().unwrap_or(&empty);
        let bucket = if tab_name.starts_with("news") {
            &mut news_items
        } else {
            &mut notice
        };
        for item in items {
            if bucket.len() >= news::MAX_ITEMS {
                break;
            }
            let title = pack_str(item, "content");
            if title.is_empty() {
                continue;
            }
            bucket.push(news::article(
                title,
                pack_str(item, "jump_url"),
                format_notice_date(&item["start_ts"]),
            ));
        }
    }

    news::envelope(notice, news_items, slideshow)
}

pub async fn get_news(profile: &Value) -> Result<Value, String> {
    let cfg = api_config(profile)?;
    let common = json!({
        "appcode": cfg.appcode,
        "language": cfg.language,
        "channel": cfg.channel,
        "sub_channel": cfg.sub_channel,
        "platform": "Windows",
        "source": "launcher",
    });
    let body = json!({
        "seq": cfg.seq,
        "proxy_reqs": [
            { "kind": "get_banner", "get_banner_req": common },
            { "kind": "get_announcement", "get_announcement_req": common },
        ],
    });
    let response = post_batch_proxy(&cfg.web_api_url, &body).await?;
    check_news_response(&response)?;
    Ok(news_from_response(&response))
}

fn check_news_response(response: &Value) -> Result<(), String> {
    if find_rsp(response, "get_banner").is_none() && find_rsp(response, "get_announcement").is_none()
    {
        return Err(format!(
            "Gryphline news response has no banner or announcement data (response: {})",
            response_snippet(response)
        ));
    }
    Ok(())
}

// ------------ Endfield Manifest ------------
// What the Endfield packs hold. An install nothing recorded is read from the latest packs' index; one that was
// recorded is read from the record, whose files come from the packs it names, or from the latest links when those are
// the same version.

async fn latest_packs(profile: &Value) -> Result<(String, Vec<Entry>), String> {
    let latest = get_latest_game(profile).await?;
    match latest["version"].as_str().filter(|v| !v.is_empty()) {
        Some(version) => Ok((version.to_string(), packs_as_entries(&latest["packs"]))),
        None => Err(format!(
            "Failed to resolve {} package metadata from the Gryphline API.",
            game_profiles::display_name(profile)
        )),
    }
}

fn archive_of(parts: &[Entry]) -> Arc<[Pack]> {
    let mut packs: Vec<Pack> = parts.iter().map(|p| Pack { url: p.url().into(), size: p.size }).collect();
    packs.sort_by(|a, b| a.url.cmp(&b.url));
    packs.into()
}

fn from_record(record: &Record, latest: Option<(String, Vec<Entry>)>) -> Manifest {
    let packs = match latest {
        Some((version, parts)) if same_version(&version, &record.version) => archive_of(&parts),
        _ => record.packs.clone().into(),
    };
    Manifest {
        version: record.version.clone(),
        entries: record.entries().into_iter().map(|file| file.member_of(&packs)).collect(),
        mirrors: packs.first().and_then(|p| super::engine::url_origin(&p.url)).into_iter().collect(),
        ..Default::default()
    }
}

pub(super) struct Hypergryph;

impl Publisher for Hypergryph {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let name = game_profiles::display_name(cx.profile);
            let record = validator::recorded(cx.dir).await;
            if cx.purpose != JobKind::Install && !record.is_empty() {
                let latest = match cx.purpose {
                    JobKind::Repair => latest_packs(cx.profile).await.ok(),
                    _ => None,
                };
                return Ok(from_record(&record, latest));
            }
            let (version, parts) = latest_packs(cx.profile).await.map_err(|e| match cx.purpose {
                JobKind::Install => e,
                _ => {
                    log::warn!("Could not fetch the latest {name} packages: {e}");
                    format!("No install manifest found for {name}, and the latest packages could not be fetched to check this install. Check your connection and try again.")
                }
            })?;
            log::info!(
                "{name} package {version} resolved with {} part(s), {:.2} GB.",
                parts.len(),
                parts.iter().map(|p| p.size).sum::<u64>() as f64 / 1_073_741_824.0
            );
            let (packs, mirrors) = (archive_of(&parts), origin_of(&parts));
            if cx.purpose != JobKind::Install {
                let entries = members::listing(&packs, &cx.job).await?;
                return Ok(Manifest { version, entries, mirrors, adopts: true, ..Default::default() });
            }
            if !super::extract::needs_unpack(cx.dir, &version) {
                return Ok(Manifest { version, mirrors, ..Default::default() });
            }
            let updating = !record.is_empty() && !same_version(&record.version, &version);
            let listed = match (members::listing(&packs, &cx.job).await, updating) {
                (Ok(entries), true) => return Ok(Manifest { version, entries, mirrors, ..Default::default() }),
                (Ok(entries), false) => entries,
                (Err(e), true) if !members::is_structural_failure(&e) => return Err(e),
                (Err(e), _) => {
                    log::warn!("{name}: the packs cannot be read file by file ({e}), so they are downloaded whole.");
                    Vec::new()
                }
            };
            Ok(Manifest { version, containers: vec![Container { parts, ..Default::default() }], mirrors, listed, ..Default::default() })
        })
    }

    fn triage(&self, dir: &Path, files: &[Entry]) -> Triage {
        Triage { trusted: Record::load(dir).vouches(dir, files), ..Triage::default() }
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({ "version": get_latest_game(profile).await?.get("version").cloned().unwrap_or(Value::Null) })) })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn write_space(&self, download: u64, install: u64) -> (u64, f64) {
        archive_space(download, install)
    }

    fn installed_version(&self, dir: &Path) -> Option<String> {
        local_game_version_at(dir).or_else(|| Record::version_at(dir))
    }

    fn policy(&self, dir: &Path) -> Policy {
        Policy { latest: Record::load(dir).is_empty(), checksums: true, records: true, nested: true, ..Policy::default() }
    }
}

// ------------ Install Preview ------------
// The packs to download and, when their index can be read, what they unpack to.
pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    let (version, parts) = latest_packs(profile).await?;
    let (install_bytes, file_count, approximate) =
        match members::listing(&archive_of(&parts), &members::Detached::job()).await {
            Ok(files) if files.iter().any(|f| f.size > 0) => (files.iter().map(|f| f.size).sum(), files.len() as u64, false),
            Ok(_) => (0, 0, true),
            Err(e) => {
                log::warn!("Endfield preview: could not read the package index: {e}");
                (0, 0, true)
            }
        };
    Ok(Preview {
        version,
        download_bytes: parts.iter().map(|r| r.size).sum(),
        install_bytes,
        file_count,
        approximate,
        parts: Vec::new(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;
    use crate::backend::manifest::Source;

    fn latest(packs: Value) -> Value {
        json!({
            "proxy_rsps": [{
                "kind": "get_latest_game",
                "get_latest_game_rsp": { "version": "1.2.3", "pkg": { "packs": packs } },
            }],
        })
    }

    fn pack(url: &str, md5: &str, size: Value) -> Value {
        json!({ "url": url, "md5": md5, "package_size": size })
    }

    #[test]
    fn valid_packs_resolve() {
        let response = latest(json!([
            pack("https://cdn.example/a.zip.001", "0123456789ABCDEF0123456789ABCDEF", json!("100")),
            pack("https://cdn.example/a.zip.002", "def", json!(50)),
        ]));
        let resolved = latest_game_from_response(&response).unwrap();
        assert_eq!(resolved["version"], "1.2.3");
        let parts = packs_as_entries(&resolved["packs"]);
        assert_eq!(parts.len(), 2);
        assert_eq!(&*parts[0].path, "a.zip.001");
        assert_eq!(parts[0].size, 100);
        assert_eq!(parts[0].hash.expected(), "0123456789abcdef0123456789abcdef");
        assert_eq!(parts[1].size, 50);
        assert!(!parts[0].live, "a pack is staged, not written into the game");
    }

    #[test]
    fn packs_missing_fields_are_rejected() {
        let cases = [
            (pack("", "abc", json!(1)), "no url"),
            (pack("https://cdn.example/a", "", json!(1)), "no md5"),
            (pack("https://cdn.example/a", "abc", json!("0")), "package_size"),
            (pack("https://cdn.example/a", "abc", json!("x")), "package_size"),
            (json!({ "url": "https://cdn.example/a", "md5": "abc" }), "package_size"),
        ];
        for (bad, needle) in cases {
            let err = latest_game_from_response(&latest(json!([bad]))).unwrap_err();
            assert!(err.contains(needle), "{err}");
        }
    }

    #[test]
    fn missing_rsp_error_carries_a_bounded_snippet() {
        let long = "é".repeat(1000);
        let response = json!({ "code": 7, "msg": long });
        let err = latest_game_from_response(&response).unwrap_err();
        assert!(err.contains("\"code\":7"), "{err}");
        assert_eq!(response_snippet(&response).chars().count(), 300);
    }

    #[test]
    fn notice_dates_accept_seconds_and_millis() {
        let seconds = format_notice_date(&json!(1_758_196_800));
        let millis = format_notice_date(&json!("1758196800000"));
        assert_eq!(seconds, millis);
        assert_eq!(seconds.len(), 5);
        assert_eq!(&seconds[2..3], "-");
        assert_eq!(format_notice_date(&Value::Null), "");
    }

    #[test]
    fn an_error_envelope_is_a_failure_not_empty_news() {
        let err = check_news_response(&json!({ "code": 503, "msg": "maintenance" })).unwrap_err();
        assert!(err.contains("\"code\":503"), "{err}");
        let partial = json!({ "proxy_rsps": [{ "kind": "get_banner", "get_banner_rsp": {} }] });
        assert!(check_news_response(&partial).is_ok());
    }

    #[test]
    fn news_is_capped_and_skips_empty_banners() {
        let banners: Vec<Value> = (0..8)
            .map(|i| json!({ "url": if i == 0 { String::new() } else { format!("https://img/{i}") }, "jump_url": "" }))
            .collect();
        let announcements: Vec<Value> = (0..20)
            .map(|i| json!({ "content": format!("item {i}"), "jump_url": "https://j", "start_ts": "1758196800" }))
            .collect();
        let response = json!({
            "proxy_rsps": [
                { "kind": "get_banner", "get_banner_rsp": { "banners": banners } },
                { "kind": "get_announcement", "get_announcement_rsp": { "tabs": [
                    { "tabName": "News", "announcements": announcements },
                    { "tabName": "Notice", "announcements": announcements },
                ] } },
            ],
        });
        let out = news_from_response(&response);
        let slides = out["slideshow"].as_array().unwrap();
        assert_eq!(slides.len(), news::MAX_SLIDES);
        assert_eq!(slides[0]["url"], "https://img/1");
        assert_eq!(
            out["guidance"]["news"]["contents"].as_array().unwrap().len(),
            news::MAX_ITEMS
        );
        assert_eq!(
            out["guidance"]["notice"]["contents"].as_array().unwrap().len(),
            news::MAX_ITEMS
        );
        assert!(out.get("social_media_list").is_none());
    }


    fn member(path: &str, data: &[u8], packs: &Arc<[Pack]>) -> Entry {
        Entry::new(path, data.len() as u64, Hash::Crc32(crc32fast::hash(data)), "").member_of(packs)
    }

    fn urls(entry: &Entry) -> Vec<String> {
        match &entry.source {
            Source::Member { packs, .. } => packs.iter().map(|p| p.url.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn a_recorded_install_is_read_from_its_recorded_packs_or_the_latest_links_of_the_same_version() {
        let dir = TempDir::new("recorded-endfield");
        dir.write("Data/a.bin", b"alpha");
        let recorded: Arc<[Pack]> = vec![Pack { url: "https://old.example/g.zip.001".into(), size: 9 }].into();
        Record::of(dir.path(), "1.0.0", &[member("Data/a.bin", b"alpha", &recorded)]).save(dir.path()).unwrap();
        let record = Record::load(dir.path());
        assert_eq!((record.packs.len(), record.is_empty()), (1, false), "the install record keeps the archive");

        let moved = packs_as_entries(&json!([
            pack("https://new.example/g.zip.002", "0123456789abcdef0123456789abcdef", json!(5)),
            pack("https://new.example/g.zip.001", "0123456789abcdef0123456789abcdef", json!(4)),
        ]));
        let same = from_record(&record, Some(("1.0.0".to_string(), moved.clone())));
        assert_eq!(urls(&same.entries[0]), ["https://new.example/g.zip.001", "https://new.example/g.zip.002"]);
        assert_eq!(same.mirrors, ["https://new.example"]);
        for other in [from_record(&record, Some(("1.1.0".to_string(), moved))), from_record(&record, None)] {
            assert_eq!(urls(&other.entries[0]), ["https://old.example/g.zip.001"], "an older install keeps its own packs");
            assert_eq!((other.version.as_str(), other.adopts), ("1.0.0", false));
        }
        let file = &same.entries[0];
        assert_eq!((&*file.path, file.size, file.hash), ("Data/a.bin", 5, Hash::Crc32(crc32fast::hash(b"alpha"))));
        assert!(matches!(file.source, Source::Member { at: None, .. }), "a record does not know where a member sits");
    }
}
