// ------------ GameBanana Mods ------------
// Talks to the GameBanana API so the Mods page can browse, search, preview, download and update mods for a game.
// Search and feed results are cached for a few minutes, and anything marked NSFW is hidden unless the setting allows it.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::BackendState;
use super::{
    arg_str, err_response, fs_util, game_profiles, http, mods, ok_response, ok_with,
    resolve_profile,
};

const API: &str = "https://gamebanana.com/apiv11";

const CLEAN_AV_RESULT: &str = "clean";

const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn is_bad_request(error: &str) -> bool {
    error == "GameBanana returned HTTP 400"
}

async fn get_json(url: &str) -> Result<Value, String> {
    http::with_retry(
        || async {
            let response = http::client()
                .get(url)
                .header("Accept", "application/json")
                .timeout(API_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("Request error: {e}"))?;
            let status = response.status().as_u16();
            if status != 200 {
                let error = format!("GameBanana returned HTTP {status}");
                return if retryable_status(status) {
                    Err(error)
                } else {
                    Ok(Err(error))
                };
            }
            let body = http::read_capped(response, url).await?;
            if body.iter().all(u8::is_ascii_whitespace) {
                return Err("GameBanana sent back an empty response".to_string());
            }
            serde_json::from_slice(&body)
                .map(Ok)
                .map_err(|e| format!("Invalid JSON from GameBanana: {e}"))
        },
        3,
        400,
        "GameBanana request",
    )
    .await?
}

fn nsfw_allowed(app: &AppHandle) -> bool {
    app.state::<BackendState>()
        .config
        .get("behavior.showNsfwMods")
        == Value::Bool(true)
}

fn is_nsfw(record: &Value) -> bool {
    record
        .get("_bHasContentRatings")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn preview_media(record: &Value) -> &[Value] {
    let Some(media) = record.get("_aPreviewMedia") else {
        return &[];
    };
    media
        .as_array()
        .or_else(|| media.get("_aImages").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn preview_url(record: &Value) -> Option<String> {
    let first = preview_media(record).first()?;
    let base = first.get("_sBaseUrl").and_then(Value::as_str)?;
    let file = ["_sFile530", "_sFile220", "_sFile100", "_sFile"]
        .iter()
        .find_map(|key| first.get(*key).and_then(Value::as_str))?;
    Some(format!("{}/{}", base.trim_end_matches('/'), file))
}

fn flatten_record(record: &Value) -> Value {
    json!({
        "id": record.get("_idRow").and_then(Value::as_u64),
        "name": record.get("_sName").and_then(Value::as_str).unwrap_or(""),
        "profileUrl": record.get("_sProfileUrl").and_then(Value::as_str),
        "version": record.get("_sVersion").and_then(Value::as_str),
        "updatedAt": record.get("_tsDateUpdated").and_then(Value::as_u64),
        "likes": record.get("_nLikeCount").and_then(Value::as_u64).unwrap_or(0),
        "views": record.get("_nViewCount").and_then(Value::as_u64).unwrap_or(0),
        "category": record
            .get("_aSubCategory")
            .or_else(|| record.get("_aRootCategory"))
            .and_then(|c| c.get("_sName"))
            .and_then(Value::as_str),
        "submitter": record
            .get("_aSubmitter")
            .and_then(|s| s.get("_sName"))
            .and_then(Value::as_str),
        "thumbnailUrl": preview_url(record),
    })
}

// ------------ Browsing And Search ------------
// The mod feed, search and category lists, including the paging and caching that stop the page from jumping around.
const DEFAULT_PER_PAGE: u64 = 16;

const MAX_PER_PAGE: u64 = 50;

const RAW_PER_PAGE: u64 = 50;

const SEARCH_PER_PAGE: u64 = 15;

const SEARCH_MAX_FETCHES: u64 = 10;

const SEARCH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

const SEARCH_CACHE_ENTRIES: usize = 8;

const UPDATE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

const THUMBNAIL_PARALLELISM: usize = 4;

const UPDATE_PARALLELISM: usize = 6;

const MULTI_CHUNK: usize = 50;

const THUMBNAIL_PROPERTIES: &str = "_idRow,_aPreviewMedia";

const MAX_RAW_FETCHES_PER_REQUEST: u64 = 8;

const FEED_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

const FEED_CACHE_ENTRIES: usize = 8;

const CATEGORY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

fn sort_alias(sort: &str) -> &'static str {
    match sort {
        "updated" => "Generic_LatestModified",
        "likes" => "Generic_MostLiked",
        "views" => "Generic_MostViewed",
        "downloads" => "Generic_MostDownloaded",
        _ => "Generic_Newest",
    }
}

fn record_count(json: &Value) -> Option<u64> {
    json.get("_aMetadata")
        .and_then(|m| m.get("_nRecordCount"))
        .and_then(Value::as_u64)
}

fn records(json: &Value) -> &[Value] {
    json.get("_aRecords")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn page_count(json: &Value, per_page: u64) -> u64 {
    let per_page = json
        .get("_aMetadata")
        .and_then(|m| m.get("_nPerpage"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(per_page);
    record_count(json)
        .map(|r| r.div_ceil(per_page).max(1))
        .unwrap_or(1)
}

struct FeedQuery {
    gb_id: u64,
    sort: String,
    query: String,
    category: Option<u64>,
}

impl FeedQuery {
    fn searching(&self) -> bool {
        !self.query.is_empty()
    }

    fn search_url(&self, page: u64) -> String {
        format!(
            "{API}/Util/Search/Results?_sSearchString={}&_idGameRow={}&_sModelName=Mod&_nPage={page}",
            urlencode(&self.query),
            self.gb_id
        )
    }

    fn url(&self, page: u64, per_page: u64) -> String {
        let mut u = format!(
            "{API}/Mod/Index?_nPage={page}&_nPerpage={per_page}\
             &_aFilters%5BGeneric_Game%5D={}&_sSort={}",
            self.gb_id,
            sort_alias(&self.sort)
        );
        if let Some(id) = self.category {
            u.push_str(&format!("&_aFilters%5BGeneric_Category%5D={id}"));
        }
        u
    }
}

#[derive(Clone)]
struct SearchWorkingSet {
    records: Arc<Vec<Value>>,
    total_matches: u64,
    truncated: bool,
    downloads: HashMap<u64, u64>,
}

static SEARCH_CACHE: http::TtlCache<SearchWorkingSet> =
    http::TtlCache::new(SEARCH_CACHE_TTL, SEARCH_CACHE_ENTRIES);

static UPDATE_CACHE: http::TtlCache<Vec<Value>> =
    http::TtlCache::new(UPDATE_CACHE_TTL, 16);

#[derive(Clone)]
struct FeedWorkingSet {
    clean: Arc<Vec<Value>>,
    raw_seen: u64,
    next_page: u64,
    total: Option<u64>,
    exhausted: bool,
    started: std::time::Instant,
}

static FEED_CACHE: http::TtlCache<FeedWorkingSet> =
    http::TtlCache::new(FEED_CACHE_TTL, FEED_CACHE_ENTRIES);

static CATEGORY_CACHE: http::TtlCache<Vec<Value>> = http::TtlCache::new(CATEGORY_CACHE_TTL, 16);

fn root_category_id(record: &Value) -> Option<u64> {
    record
        .get("_aRootCategory")?
        .get("_sProfileUrl")?
        .as_str()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

fn number(record: &Value, key: &str) -> u64 {
    record.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn search_rank(record: &Value, sort: &str, downloads: &HashMap<u64, u64>) -> u64 {
    match sort {
        "updated" => number(record, "_tsDateModified").max(number(record, "_tsDateUpdated")),
        "likes" => number(record, "_nLikeCount"),
        "views" => number(record, "_nViewCount"),
        "downloads" => record
            .get("_idRow")
            .and_then(Value::as_u64)
            .and_then(|id| downloads.get(&id).copied())
            .unwrap_or(0),
        _ => number(record, "_tsDateAdded"),
    }
}

async fn download_counts(ids: &[u64]) -> HashMap<u64, u64> {
    let records = match mod_multi(ids, "_idRow,_nDownloadCount").await {
        Ok(records) => records,
        Err(e) => {
            log::warn!("gamebanana: download counts unavailable: {e}");
            return HashMap::new();
        }
    };
    records
        .into_iter()
        .filter_map(|(id, record)| Some((id, record.get("_nDownloadCount")?.as_u64()?)))
        .collect()
}

async fn search_working_set(q: &FeedQuery) -> Result<(Vec<Value>, u64, bool), String> {
    let first = get_json(&q.search_url(1)).await?;
    let total_matches = record_count(&first).unwrap_or(0);
    let available = total_matches.div_ceil(SEARCH_PER_PAGE).max(1);
    let pages = available.min(SEARCH_MAX_FETCHES);

    let mut records_out: Vec<Value> = records(&first).to_vec();
    if pages > 1 {
        let requests = (2..=pages).map(|p| {
            let url = q.search_url(p);
            async move { get_json(&url).await }
        });
        for result in futures::future::join_all(requests).await {
            match result {
                Ok(json) => records_out.extend(records(&json).iter().cloned()),
                Err(e) => log::warn!("gamebanana: a search page failed: {e}"),
            }
        }
    }

    let mut seen = HashSet::new();
    records_out.retain(|r| {
        r.get("_idRow")
            .and_then(Value::as_u64)
            .is_some_and(|id| seen.insert(id))
    });

    Ok((records_out, total_matches, available > pages))
}

async fn search_feed(
    app: &AppHandle,
    q: &FeedQuery,
    page: u64,
    per_page: u64,
) -> Result<Value, String> {
    let key = format!("{}:{}", q.gb_id, q.query.to_lowercase());
    let mut working = match SEARCH_CACHE.get(&key) {
        Some(hit) => hit,
        None => match search_working_set(q).await {
            Ok((fetched, total_matches, truncated)) => {
                let working = SearchWorkingSet {
                    records: Arc::new(fetched),
                    total_matches,
                    truncated,
                    downloads: HashMap::new(),
                };
                SEARCH_CACHE.set(&key, working.clone());
                working
            }
            Err(e) => {
                log::warn!("gamebanana: search failed: {e}");
                return Ok(err_response(e));
            }
        },
    };

    if q.sort == "downloads" && working.downloads.is_empty() {
        let ids: Vec<u64> = working
            .records
            .iter()
            .filter_map(|r| r.get("_idRow").and_then(Value::as_u64))
            .collect();
        working.downloads = download_counts(&ids).await;
        SEARCH_CACHE.set(&key, working.clone());
    }
    let SearchWorkingSet {
        records: records_out,
        total_matches,
        truncated,
        downloads,
    } = working;

    let allow_nsfw = nsfw_allowed(app);
    let mut matches: Vec<&Value> = records_out
        .iter()
        .filter(|r| allow_nsfw || !is_nsfw(r))
        .filter(|r| match q.category {
            Some(id) => root_category_id(r) == Some(id),
            None => true,
        })
        .collect();

    if q.sort != "relevance" {
        matches.sort_by_key(|r| std::cmp::Reverse(search_rank(r, &q.sort, &downloads)));
    }

    let mods: Vec<Value> = matches
        .iter()
        .skip(((page - 1) * per_page) as usize)
        .take(per_page as usize)
        .map(|r| flatten_record(r))
        .collect();

    Ok(ok_with(json!({
        "mods": mods,
        "totalPages": (matches.len() as u64).div_ceil(per_page).max(1),
        "hasMore": (matches.len() as u64) > page * per_page,
        "matches": matches.len(),
        "totalMatches": total_matches,
        "truncated": truncated,
    })))
}

fn visible_page_count(total: Option<u64>, clean: u64, raw_seen: u64, per_page: u64) -> u64 {
    let Some(total) = total else { return 1 };
    if raw_seen == 0 {
        return total.div_ceil(per_page).max(1);
    }
    let ratio = clean as f64 / raw_seen as f64;
    ((total as f64 * ratio).round() as u64)
        .div_ceil(per_page)
        .max(1)
}

pub(super) async fn feed(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let Some(gb_id) = game_profiles::mod_gamebanana_id(profile) else {
        return Ok(err_response(format!(
            "{} has no GameBanana page.",
            game_profiles::display_name(profile)
        )));
    };

    let page = args.get(1).and_then(Value::as_u64).unwrap_or(1).max(1);
    let q = FeedQuery {
        gb_id,
        sort: arg_str(args, 2).unwrap_or("new").to_string(),
        query: arg_str(args, 3).unwrap_or("").trim().to_string(),
        category: args.get(4).and_then(Value::as_u64).filter(|c| *c > 0),
    };
    let per_page = args
        .get(5)
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);

    if q.searching() {
        return search_feed(app, &q, page, per_page).await;
    }

    if nsfw_allowed(app) {
        let json = match get_json(&q.url(page, per_page)).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("gamebanana: feed request failed: {e}");
                return Ok(err_response(e));
            }
        };
        let mods: Vec<Value> = records(&json).iter().map(flatten_record).collect();
        let total_pages = page_count(&json, per_page);
        return Ok(ok_with(json!({
            "mods": mods,
            "totalPages": total_pages,
            "hasMore": page < total_pages,
        })));
    }

    let key = format!("{}:{}:{}", q.gb_id, q.sort, q.category.unwrap_or(0));
    let cached = FEED_CACHE
        .get(&key)
        .filter(|w| w.started.elapsed() < FEED_CACHE_TTL);
    let mut working = match cached {
        Some(hit) => hit,
        None => {
            let first = match get_json(&q.url(1, RAW_PER_PAGE)).await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("gamebanana: feed request failed: {e}");
                    return Ok(err_response(e));
                }
            };
            let batch = records(&first);
            let total = record_count(&first);
            FeedWorkingSet {
                clean: Arc::new(batch.iter().filter(|r| !is_nsfw(r)).map(flatten_record).collect()),
                raw_seen: batch.len() as u64,
                next_page: 2,
                total,
                exhausted: batch.is_empty() || raw_page_count(total).is_some_and(|pages| pages < 2),
                started: std::time::Instant::now(),
            }
        }
    };

    let wanted = page * per_page;
    let failure = extend_feed(&q, &mut working, wanted).await;
    FEED_CACHE.set(&key, working.clone());

    let clean_len = working.clean.len() as u64;
    let skip = (page - 1) * per_page;
    if let Some(e) = failure {
        if clean_len <= skip {
            return Ok(err_response(e));
        }
    }
    let (total_pages, has_more) = feed_paging(&working, page, per_page);
    let mods: Vec<Value> = working
        .clean
        .iter()
        .skip(skip as usize)
        .take(per_page as usize)
        .cloned()
        .collect();

    Ok(ok_with(json!({
        "mods": mods,
        "totalPages": total_pages,
        "hasMore": has_more,
    })))
}

fn raw_page_count(total: Option<u64>) -> Option<u64> {
    total.map(|t| t.div_ceil(RAW_PER_PAGE))
}

fn feed_paging(working: &FeedWorkingSet, page: u64, per_page: u64) -> (u64, bool) {
    let clean_len = working.clean.len() as u64;
    let loaded_pages = clean_len.div_ceil(per_page).max(1);
    if working.exhausted {
        return (loaded_pages, clean_len > page * per_page);
    }
    let estimate = visible_page_count(working.total, clean_len, working.raw_seen, per_page);
    (estimate.max(loaded_pages).max(page + 1), true)
}

async fn extend_feed(q: &FeedQuery, working: &mut FeedWorkingSet, wanted: u64) -> Option<String> {
    let budget_end = working.next_page + MAX_RAW_FETCHES_PER_REQUEST - 1;
    let mut failure = None;
    while (working.clean.len() as u64) < wanted
        && !working.exhausted
        && working.next_page <= budget_end
    {
        let ratio = if working.raw_seen > 0 {
            (working.clean.len() as f64 / working.raw_seen as f64).max(0.2)
        } else {
            1.0
        };
        let short = wanted - working.clean.len() as u64;
        let needed = ((short as f64) / (ratio * RAW_PER_PAGE as f64)).ceil() as u64;
        let mut last = (working.next_page + needed.max(1) - 1).min(budget_end);
        if let Some(pages) = raw_page_count(working.total) {
            last = last.min(pages);
        }
        if last < working.next_page {
            working.exhausted = true;
            break;
        }
        let requests = (working.next_page..=last).map(|p| {
            let url = q.url(p, RAW_PER_PAGE);
            async move { get_json(&url).await }
        });
        let results = futures::future::join_all(requests).await;
        let clean = Arc::make_mut(&mut working.clean);
        let mut seen: HashSet<u64> = clean
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_u64))
            .collect();
        for result in results {
            match result {
                Ok(json) => {
                    let batch = records(&json);
                    working.next_page += 1;
                    if batch.is_empty() {
                        working.exhausted = true;
                        break;
                    }
                    working.raw_seen += batch.len() as u64;
                    clean.extend(
                        batch
                            .iter()
                            .filter(|r| !is_nsfw(r))
                            .map(flatten_record)
                            .filter(|r| {
                                r.get("id")
                                    .and_then(Value::as_u64)
                                    .is_none_or(|id| seen.insert(id))
                            }),
                    );
                }
                Err(e) => {
                    log::warn!("gamebanana: a feed page failed: {e}");
                    failure = Some(e);
                    break;
                }
            }
        }
        if failure.is_some() {
            break;
        }
        if raw_page_count(working.total).is_some_and(|pages| working.next_page > pages) {
            working.exhausted = true;
        }
    }
    failure
}

pub(super) async fn categories(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let Some(gb_id) = game_profiles::mod_gamebanana_id(profile) else {
        return Ok(ok_with(json!({ "categories": [] })));
    };

    let cache_key = gb_id.to_string();
    if let Some(categories) = CATEGORY_CACHE.get(&cache_key) {
        return Ok(ok_with(json!({ "categories": categories })));
    }

    let json = match get_json(&format!("{API}/Game/{gb_id}/ProfilePage")).await {
        Ok(v) => v,
        Err(e) => return Ok(err_response(e)),
    };

    let categories: Vec<Value> = json
        .get("_aModRootCategories")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    Some(json!({
                        "id": c.get("_idRow")?.as_u64()?,
                        "name": c.get("_sName")?.as_str()?,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    CATEGORY_CACHE.set(&cache_key, categories.clone());
    Ok(ok_with(json!({ "categories": categories })))
}

// ------------ Mod Details ------------
// Fetches one mod's page: description, preview images and downloadable files.
pub(super) async fn mod_profile(_app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(mod_id) = args.first().and_then(Value::as_u64) else {
        return Ok(err_response("No mod was specified."));
    };

    let profile_url = format!("{API}/Mod/{mod_id}/ProfilePage");
    let downloads_url = format!("{API}/Mod/{mod_id}/DownloadPage");
    let (profile, downloads) = tokio::join!(get_json(&profile_url), get_json(&downloads_url));
    let profile = match profile {
        Ok(v) => v,
        Err(e) => return Ok(err_response(e)),
    };
    let (files, files_error) = match downloads {
        Ok(d) => (flatten_files(&d), None),
        Err(e) => {
            log::warn!("gamebanana: file list for mod {mod_id} failed: {e}");
            (Vec::new(), Some(e))
        }
    };

    let mut reply = flatten_record(&profile);
    reply["id"] = json!(mod_id);
    reply["updatedAt"] = json!(profile.get("_tsDateModified").and_then(Value::as_u64));
    reply["tagline"] = json!(profile.get("_sDescription").and_then(Value::as_str));
    reply["description"] = json!(profile
        .get("_sText")
        .and_then(Value::as_str)
        .map(html_to_text)
        .unwrap_or_default());
    reply["images"] = json!(preview_images(&profile));
    reply["files"] = json!(files);
    reply["filesError"] = json!(files_error);
    Ok(ok_with(reply))
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn html_to_text(html: &str) -> String {
    const BLOCK_TAGS: [&str; 12] = [
        "br",
        "p",
        "div",
        "li",
        "tr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "blockquote",
    ];
    const VOID_CONTENT_TAGS: [&str; 2] = ["script", "style"];

    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut tag = String::new();
    let mut skipping: Option<&str> = None;

    while let Some(c) = chars.next() {
        if c != '<' {
            if skipping.is_none() {
                out.push(c);
            }
            continue;
        }
        tag.clear();
        for t in chars.by_ref() {
            if t == '>' {
                break;
            }
            tag.push(t);
        }
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        if let Some(open) = skipping {
            if closing && name == open {
                skipping = None;
            }
            continue;
        }
        if let Some(t) = VOID_CONTENT_TAGS.iter().find(|t| **t == name) {
            if !closing {
                skipping = Some(t);
            }
            continue;
        }
        let line_only = matches!(name.as_str(), "li" | "tr");
        if BLOCK_TAGS.contains(&name.as_str()) && !(closing && line_only) {
            out.push('\n');
        }
    }

    let decoded = decode_entities(&out);
    let mut lines: Vec<&str> = Vec::new();
    for line in decoded.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn decode_entities(text: &str) -> String {
    const NAMED: [(&str, char); 22] = [
        ("nbsp", ' '),
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("rsquo", '\u{2019}'),
        ("lsquo", '\u{2018}'),
        ("rdquo", '\u{201D}'),
        ("ldquo", '\u{201C}'),
        ("hellip", '\u{2026}'),
        ("mdash", '\u{2014}'),
        ("ndash", '\u{2013}'),
        ("laquo", '\u{00AB}'),
        ("raquo", '\u{00BB}'),
        ("bull", '\u{2022}'),
        ("middot", '\u{00B7}'),
        ("copy", '\u{00A9}'),
        ("reg", '\u{00AE}'),
        ("trade", '\u{2122}'),
        ("deg", '\u{00B0}'),
        ("times", '\u{00D7}'),
    ];

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest[1..]
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| {
                let entity = &rest[1..=end];
                let c = if let Some(num) = entity.strip_prefix('#') {
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => num.parse::<u32>().ok(),
                    }?;
                    if code == 0xA0 {
                        ' '
                    } else {
                        char::from_u32(code).filter(|c| *c != '\0')?
                    }
                } else {
                    NAMED.iter().find(|(name, _)| *name == entity)?.1
                };
                Some((c, end + 2))
            });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn preview_images(record: &Value) -> Vec<Value> {
    preview_media(record)
        .iter()
        .filter_map(|img| {
            let base = img.get("_sBaseUrl")?.as_str()?.trim_end_matches('/');
            let full = ["_sFile", "_sFile530", "_sFile220"]
                .iter()
                .find_map(|k| img.get(*k).and_then(Value::as_str))?;
            let thumb = ["_sFile220", "_sFile100", "_sFile530", "_sFile"]
                .iter()
                .find_map(|k| img.get(*k).and_then(Value::as_str))?;
            let hero = ["_sFile530", "_sFile220", "_sFile"]
                .iter()
                .find_map(|k| img.get(*k).and_then(Value::as_str))?;
            Some(json!({
                "url": format!("{base}/{full}"),
                "thumbUrl": format!("{base}/{thumb}"),
                "heroUrl": format!("{base}/{hero}"),
            }))
        })
        .collect()
}

fn flatten_files(download_page: &Value) -> Vec<Value> {
    download_page
        .get("_aFiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|f| {
            let av = f
                .get("_sAvResult")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            json!({
                "id": f.get("_idRow").and_then(Value::as_u64),
                "fileName": f.get("_sFile").and_then(Value::as_str).unwrap_or(""),
                "sizeBytes": f.get("_nFilesize").and_then(Value::as_u64).unwrap_or(0),
                "version": f.get("_sVersion").and_then(Value::as_str),
                "description": f.get("_sDescription").and_then(Value::as_str),
                "avResult": av,
                "installable": is_clean_file(f),
                "dateAdded": f.get("_tsDateAdded").and_then(Value::as_u64),
                "family": file_family(f.get("_sFile").and_then(Value::as_str).unwrap_or("")),
            })
        })
        .collect()
}

// ------------ Thumbnails ------------
// Fills in missing preview pictures for mods that are already installed.
pub(super) async fn backfill_thumbnails(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile).to_string();

    use futures::StreamExt;

    let missing = mods::mods_missing_thumbnails(app, &game_id);
    if missing.is_empty() {
        return Ok(ok_with(json!({ "updated": 0 })));
    }
    let mut ids: Vec<u64> = missing.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    ids.dedup();

    let batches = ids.chunks(MULTI_CHUNK).map(|chunk| async move {
        (chunk, mod_multi(chunk, THUMBNAIL_PROPERTIES).await)
    });
    let mut records = HashMap::new();
    let mut answered: HashSet<u64> = HashSet::new();
    let mut refused: Vec<Vec<u64>> = Vec::new();
    for (chunk, result) in futures::future::join_all(batches).await {
        sort_thumbnail_batch(chunk, result, &mut records, &mut answered, &mut refused);
    }
    while !refused.is_empty() && http::is_online_cached() {
        let halves: Vec<Vec<u64>> = refused
            .drain(..)
            .flat_map(|mut left| {
                let right = left.split_off(left.len() / 2);
                [left, right]
            })
            .collect();
        let results: Vec<_> = futures::stream::iter(halves)
            .map(|half| async move {
                let result = mod_multi(&half, THUMBNAIL_PROPERTIES).await;
                (half, result)
            })
            .buffer_unordered(THUMBNAIL_PARALLELISM)
            .collect()
            .await;
        for (half, result) in results {
            sort_thumbnail_batch(&half, result, &mut records, &mut answered, &mut refused);
        }
    }

    let thumbnails = thumbnail_answers(missing, &records, &answered);
    let updated = thumbnails.iter().filter(|(_, url)| url.is_some()).count();

    if !thumbnails.is_empty() {
        let blocking_app = app.clone();
        let blocking_game = game_id.clone();
        tauri::async_runtime::spawn_blocking(move || {
            mods::set_mod_thumbnails(&blocking_app, &blocking_game, &thumbnails);
        })
        .await
        .map_err(|e| e.to_string())?;
    }

    if updated > 0 {
        log::info!("gamebanana: backfilled {updated} mod thumbnail(s) for {game_id}");
    }
    Ok(ok_with(json!({ "updated": updated })))
}

fn sort_thumbnail_batch(
    ids: &[u64],
    result: Result<HashMap<u64, Value>, String>,
    records: &mut HashMap<u64, Value>,
    answered: &mut HashSet<u64>,
    refused: &mut Vec<Vec<u64>>,
) {
    match result {
        Ok(found) => {
            answered.extend(ids);
            records.extend(found);
        }
        Err(e) if is_bad_request(&e) && ids.len() > 1 => refused.push(ids.to_vec()),
        Err(e) if is_bad_request(&e) => answered.extend(ids),
        Err(_) => {}
    }
}

fn thumbnail_answers(
    missing: Vec<(String, u64)>,
    records: &HashMap<u64, Value>,
    answered: &HashSet<u64>,
) -> Vec<(String, Option<String>)> {
    missing
        .into_iter()
        .filter_map(|(folder, mod_id)| match records.get(&mod_id).and_then(preview_url) {
            Some(url) => Some((folder, Some(url))),
            None if answered.contains(&mod_id) => Some((folder, None)),
            None => None,
        })
        .collect()
}

// ------------ Installing Mods ------------
// Downloads a mod file from GameBanana, checks the archive and hands it to the mod library to install.
pub(crate) async fn install(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    install_with_cancel(app, args, None).await
}

pub(crate) async fn install_with_cancel(
    app: &AppHandle,
    args: &[Value],
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Value, String> {
    let id_text = |index: usize| {
        args.get(index)
            .and_then(Value::as_u64)
            .map_or_else(|| "none".to_string(), |id| id.to_string())
    };
    log::info!(
        "gamebanana: install requested (game {}, mod {}, file {})",
        arg_str(args, 0).unwrap_or("active"),
        id_text(1),
        id_text(2),
    );
    let outcome = install_inner(app, args, cancel).await;
    match &outcome {
        Ok(value) if value.get("cancelled") == Some(&Value::Bool(true)) => {
            log::info!("gamebanana: install cancelled");
        }
        Ok(value) if value.get("success") == Some(&Value::Bool(false)) => {
            let reason = value.get("error").and_then(Value::as_str).unwrap_or("");
            log::warn!("gamebanana: install refused: {reason}");
        }
        Err(e) => log::warn!("gamebanana: install failed: {e}"),
        _ => {}
    }
    outcome
}

fn cancelled_response() -> Value {
    json!({ "success": false, "cancelled": true })
}

pub(super) async fn cancel_install(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile);
    let target = match (
        args.get(1).and_then(Value::as_u64),
        args.get(2).and_then(Value::as_u64),
    ) {
        (Some(mod_id), Some(file_id)) => Some((mod_id, file_id)),
        _ => None,
    };
    let stopped = cancel_fetches(game_id, target);
    log::info!("gamebanana: cancel requested for {stopped} {game_id} mod download(s)");
    Ok(ok_response())
}

fn cancel_fetches(game_id: &str, target: Option<(u64, u64)>) -> usize {
    let active = IN_FLIGHT.lock();
    let mut stopped = 0;
    for fetch in active.iter().filter(|f| f.game_id == game_id) {
        let hit = match target {
            Some(key) => fetch.key == key && !fetch.caller_cancel,
            None => !fetch.caller_cancel,
        };
        if hit {
            fetch.cancel.store(true, Ordering::SeqCst);
            stopped += 1;
        }
    }
    stopped
}

async fn install_inner(
    app: &AppHandle,
    args: &[Value],
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Value, String> {
    if !mods::master_enabled(app) {
        return Ok(err_response("Turn on mod support first."));
    }

    let profile = resolve_profile(app, arg_str(args, 0));
    let profile_id = game_profiles::profile_id(profile).to_string();
    let Some(variant) = game_profiles::mod_variant(profile).map(str::to_string) else {
        return Ok(err_response(format!(
            "{} can't use mods.",
            game_profiles::display_name(profile)
        )));
    };
    let (Some(mod_id), Some(file_id)) = (
        args.get(1).and_then(Value::as_u64),
        args.get(2).and_then(Value::as_u64),
    ) else {
        return Ok(err_response("No mod file was specified."));
    };

    let title = arg_str(args, 3)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let label = title.clone().unwrap_or_else(|| "the mod".to_string());
    let publish =
        |message: &str, percent: f64| mods::publish_progress(app, &profile_id, message, percent);

    let existing: Vec<mods::GbEntry> = mods::gamebanana_entries(app, &profile_id)
        .into_iter()
        .filter(|e| e.gb_mod_id == mod_id && e.gb_file_id == file_id)
        .collect();
    if !existing.is_empty() {
        log::info!(
            "gamebanana: file {file_id} of mod {mod_id} is already installed for {profile_id}, skipping the download"
        );
        let folders: Vec<&str> = existing.iter().map(|e| e.folder.as_str()).collect();
        return Ok(ok_with(json!({
            "alreadyInstalled": true,
            "installed": folders,
            "folder": folders[0],
            "enabled": existing.iter().any(|e| e.enabled),
        })));
    }

    let fetched = match fetch_gb_file(app, &profile_id, mod_id, file_id, &label, cancel).await {
        Ok(fetched) => fetched,
        Err(e) if e == fs_util::CANCELLED_MSG => return Ok(cancelled_response()),
        Err(e) => return Ok(err_response(e)),
    };
    if fetched.cancel_requested(app, &profile_id) {
        return Ok(cancelled_response());
    }

    let installing = format!("Installing {label}…");
    publish(&installing, INSTALL_PERCENT);
    let thumbnail = arg_str(args, 4)
        .map(str::to_string)
        .or_else(|| preview_url(&fetched.listing));
    let source = gb_source(&fetched, mod_id, file_id, thumbnail);

    let result = mods::install_archive(
        app,
        &profile_id,
        &variant,
        &fetched.archive,
        source,
        mods::ArchiveOptions {
            label: title.as_deref(),
            progress: Some((&installing, INSTALL_PERCENT, 99.0)),
        },
    )
    .await;
    let _ = std::fs::remove_dir_all(&fetched.staging);

    match result {
        Ok(installed) => {
            mods::finish_progress(app, &profile_id, false);
            let ids = mods::mod_ids_for_folders(app, &profile_id, &installed);
            super::mod_profiles::add_to_active(app, &profile_id, &ids);
            mods::notify_mods_changed(app, &profile_id);
            Ok(ok_with(json!({ "installed": installed })))
        }
        Err(e) => {
            mods::finish_progress(app, &profile_id, true);
            Ok(err_response(e))
        }
    }
}

struct ActiveFetch {
    key: (u64, u64),
    game_id: String,
    cancel: Arc<AtomicBool>,
    caller_cancel: bool,
}

static IN_FLIGHT: parking_lot::Mutex<Vec<ActiveFetch>> = parking_lot::Mutex::new(Vec::new());

pub(super) fn any_in_flight() -> bool {
    !IN_FLIGHT.lock().is_empty()
}

struct InFlight {
    key: (u64, u64),
    cancel: Arc<AtomicBool>,
}

impl InFlight {
    fn claim(
        game_id: &str,
        mod_id: u64,
        file_id: u64,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Option<Self> {
        let key = (mod_id, file_id);
        let mut active = IN_FLIGHT.lock();
        if active.iter().any(|f| f.key == key) {
            return None;
        }
        let caller_cancel = cancel.is_some();
        let cancel = cancel.unwrap_or_default();
        active.push(ActiveFetch {
            key,
            game_id: game_id.to_string(),
            cancel: cancel.clone(),
            caller_cancel,
        });
        Some(Self { key, cancel })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.lock().retain(|f| f.key != self.key);
    }
}

struct Fetched {
    staging: PathBuf,
    archive: PathBuf,
    listing: Value,
    file: Value,
    _claim: InFlight,
}

const INSTALL_PERCENT: f64 = 90.0;

impl Fetched {
    fn cancel_requested(&self, app: &AppHandle, game_id: &str) -> bool {
        if !self._claim.cancel.load(Ordering::SeqCst) {
            return false;
        }
        let _ = std::fs::remove_dir_all(&self.staging);
        mods::finish_progress(app, game_id, true);
        true
    }
}

fn final_download_error(e: &str) -> bool {
    e == fs_util::CANCELLED_MSG
        || e == OVERSIZED_DOWNLOAD
        || e == TOO_LARGE_DOWNLOAD
        || e
            .strip_prefix("GameBanana returned ")
            .and_then(http::permanent_client_status)
            .is_some()
        || fs_util::classify(e) == fs_util::FailureKind::DiskFull
}

fn gb_source(fetched: &Fetched, mod_id: u64, file_id: u64, thumbnail: Option<String>) -> Value {
    json!({
        "kind": "gamebanana",
        "gbModId": mod_id,
        "gbFileId": file_id,
        "version": fetched.file.get("_sVersion").and_then(Value::as_str),
        "thumbnailUrl": thumbnail,
    })
}

fn is_clean_file(file: &Value) -> bool {
    file.get("_sAvResult")
        .and_then(Value::as_str)
        .is_some_and(|av| av.eq_ignore_ascii_case(CLEAN_AV_RESULT))
}

fn file_family(name: &str) -> String {
    const VERSION_WORDS: [&str; 5] = ["v", "ver", "version", "update", "updated"];
    let lower = name.to_lowercase();
    let stem = match lower.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => lower.as_str(),
    };
    stem.split(|c: char| !c.is_alphabetic())
        .filter(|token| !token.is_empty() && !VERSION_WORDS.contains(token))
        .collect::<Vec<_>>()
        .join(" ")
}

fn update_target(listing: &Value, installed_file_id: u64) -> Option<&Value> {
    let files = listing.get("_aFiles")?.as_array()?;
    let clean = files.iter().filter(|f| is_clean_file(f));
    let installed = files
        .iter()
        .find(|f| f.get("_idRow").and_then(Value::as_u64) == Some(installed_file_id));
    let Some(installed) = installed else {
        return clean.max_by_key(|f| number(f, "_tsDateAdded"));
    };
    let family = file_family(installed.get("_sFile").and_then(Value::as_str).unwrap_or(""));
    let installed_at = number(installed, "_tsDateAdded");
    clean
        .filter(|f| f.get("_idRow").and_then(Value::as_u64) != Some(installed_file_id))
        .filter(|f| number(f, "_tsDateAdded") > installed_at)
        .filter(|f| file_family(f.get("_sFile").and_then(Value::as_str).unwrap_or("")) == family)
        .max_by_key(|f| number(f, "_tsDateAdded"))
}

async fn fetch_gb_file(
    app: &AppHandle,
    profile_id: &str,
    mod_id: u64,
    file_id: u64,
    label: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Fetched, String> {
    let claim = InFlight::claim(profile_id, mod_id, file_id, cancel)
        .ok_or_else(|| format!("{label} is already downloading. Wait for it to finish."))?;
    let cancel = claim.cancel.clone();
    let publish =
        |message: &str, percent: f64| mods::publish_progress(app, profile_id, message, percent);

    let listing = get_json(&format!("{API}/Mod/{mod_id}/DownloadPage")).await?;
    let file = listing
        .get("_aFiles")
        .and_then(Value::as_array)
        .and_then(|files| {
            files
                .iter()
                .find(|f| f.get("_idRow").and_then(Value::as_u64) == Some(file_id))
        })
        .cloned()
        .ok_or("That file is no longer available.")?;

    let av = file
        .get("_sAvResult")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !av.eq_ignore_ascii_case(CLEAN_AV_RESULT) {
        log::warn!("gamebanana: refusing file {file_id} — AV result \"{av}\"");
        return Err(format!(
            "GameBanana's virus scan reported \"{av}\" for this file, so Peebify won't install it."
        ));
    }

    let download_url = file
        .get("_sDownloadUrl")
        .and_then(Value::as_str)
        .ok_or("That file has no download link.")?
        .to_string();
    if !download_url.starts_with("https://") {
        log::warn!("gamebanana: refusing file {file_id} — download link is not HTTPS ({download_url})");
        return Err("GameBanana gave a download link that is not secure, so Peebify won't install it.".to_string());
    }
    let file_name = archive_file_name(
        file.get("_sFile")
            .and_then(Value::as_str)
            .unwrap_or("mod.zip"),
    );
    let expected_md5 = file
        .get("_sMd5Checksum")
        .and_then(Value::as_str)
        .map(str::to_string);
    let listed_size = file.get("_nFilesize").and_then(Value::as_u64).unwrap_or(0);

    publish(&format!("Downloading {label}…"), 1.0);

    let state = app.state::<BackendState>();
    let downloads_root = state.user_data.join("mods").join(".downloads");
    mods::prune_stale_staging(
        &downloads_root,
        std::time::Duration::from_secs(60 * 60 * 24),
    );
    let staging: PathBuf = downloads_root.join(format!(
        "{mod_id}-{file_id}-{}",
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::create_dir_all(&staging).map_err(|e| format!("Could not prepare a download: {e}"))?;
    let archive = staging.join(&file_name);

    let downloaded = http::with_retry(
        || async {
            let attempt = async {
                download(&download_url, &archive, listed_size, &cancel, |p| {
                    publish(&format!("Downloading {label}…"), (p * 0.8).clamp(1.0, 99.0))
                })
                .await?;
                archive_sanity_check(&archive)
            }
            .await;
            match attempt {
                Err(e) if final_download_error(&e) => Ok(Err(e)),
                other => other.map(Ok),
            }
        },
        3,
        1000,
        "GameBanana mod download",
    )
    .await
    .and_then(|r| r);
    let abandon = || {
        let _ = std::fs::remove_dir_all(&staging);
        mods::finish_progress(app, profile_id, true);
    };
    if let Err(e) = downloaded {
        if e == fs_util::CANCELLED_MSG {
            log::info!("gamebanana: download of {file_name} cancelled");
        }
        abandon();
        return Err(e);
    }

    if let Some(expected) = expected_md5.as_deref() {
        publish(&format!("Verifying {label}…"), 85.0);
        let hash_path = archive.clone();
        let hash_cancel = cancel.clone();
        let hashed = tauri::async_runtime::spawn_blocking(move || {
            fs_util::md5_file(&hash_path, &mut || hash_cancel.load(Ordering::SeqCst), &mut |_| {})
        })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match hashed {
            Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
            Err(e) if e == fs_util::CANCELLED_MSG => {
                log::info!("gamebanana: download of {file_name} cancelled while it was verified");
                abandon();
                return Err(e);
            }
            Ok(actual) => {
                abandon();
                log::warn!("gamebanana: md5 mismatch for {file_name} ({actual} != {expected})");
                return Err(
                    "The downloaded file didn't match GameBanana's checksum, so it wasn't installed."
                        .to_string(),
                );
            }
            Err(e) => {
                log::warn!("gamebanana: could not hash {file_name}: {e}");
            }
        }
    }

    Ok(Fetched {
        staging,
        archive,
        listing,
        file,
        _claim: claim,
    })
}

async fn mod_multi(ids: &[u64], properties: &str) -> Result<HashMap<u64, Value>, String> {
    let requests = ids.chunks(MULTI_CHUNK).map(|chunk| {
        let csv = chunk
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("{API}/Mod/Multi?_csvRowIds={csv}&_csvProperties={properties}");
        async move { (csv, get_json(&url).await) }
    });
    let mut out = HashMap::new();
    let mut succeeded = 0usize;
    let mut last_error = None;
    for (csv, result) in futures::future::join_all(requests).await {
        let json = match result {
            Ok(json) => json,
            Err(e) => {
                log::info!("gamebanana: skipped a batch of mods ({csv}) that GameBanana refused: {e}");
                last_error = Some(e);
                continue;
            }
        };
        succeeded += 1;
        for record in json.as_array().map(Vec::as_slice).unwrap_or_default() {
            if let Some(id) = record.get("_idRow").and_then(Value::as_u64) {
                out.insert(id, record.clone());
            }
        }
    }
    match last_error {
        Some(e) if succeeded == 0 => Err(e),
        _ => Ok(out),
    }
}

// ------------ Mod Updates ------------
// Checks whether newer files exist for installed GameBanana mods and updates them on request.
pub(super) async fn check_updates(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    use futures::StreamExt;

    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile).to_string();
    let force = args.get(1).and_then(Value::as_bool).unwrap_or(false);

    if !force {
        if let Some(cached) = UPDATE_CACHE.get(&game_id) {
            return Ok(ok_with(json!({ "updates": cached })));
        }
    }

    let entries = mods::gamebanana_entries(app, &game_id);
    if entries.is_empty() {
        UPDATE_CACHE.set(&game_id, Vec::new());
        return Ok(ok_with(json!({ "updates": [] })));
    }

    let ids: Vec<u64> = entries
        .iter()
        .map(|e| e.gb_mod_id)
        .collect::<HashSet<u64>>()
        .into_iter()
        .collect();
    let records = match mod_multi(&ids, "_idRow,_tsDateUpdated,_tsDateModified,_sVersion").await {
        Ok(records) => records,
        Err(e) => {
            log::warn!("gamebanana: update check failed: {e}");
            return Ok(err_response(e));
        }
    };

    let candidates: Vec<(usize, u64)> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let Some(record) = records.get(&entry.gb_mod_id) else {
                return false;
            };
            let updated = number(record, "_tsDateUpdated").max(number(record, "_tsDateModified"));
            match entry.installed_at {
                Some(installed) => updated as i64 > installed,
                None => true,
            }
        })
        .map(|(index, entry)| (index, entry.gb_mod_id))
        .collect();

    let checks: Vec<(usize, Result<Value, String>)> = futures::stream::iter(candidates)
        .map(|(index, gb_mod_id)| async move {
            let listing = get_json(&format!("{API}/Mod/{gb_mod_id}/DownloadPage")).await;
            (index, listing)
        })
        .buffer_unordered(UPDATE_PARALLELISM)
        .collect()
        .await;

    let mut updates = Vec::new();
    for (index, listing) in checks {
        let entry = &entries[index];
        let listing = match listing {
            Ok(listing) => listing,
            Err(e) => {
                log::warn!("gamebanana: could not read files for \"{}\": {e}", entry.name);
                continue;
            }
        };
        let Some(newest) = update_target(&listing, entry.gb_file_id) else {
            continue;
        };
        let Some(newest_id) = newest.get("_idRow").and_then(Value::as_u64) else {
            continue;
        };
        if newest_id == entry.gb_file_id {
            continue;
        }
        updates.push(json!({
            "modId": entry.mod_id,
            "name": entry.name,
            "fileId": newest_id,
            "version": newest
                .get("_sVersion")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .or_else(|| records.get(&entry.gb_mod_id)?.get("_sVersion")?.as_str()),
        }));
    }
    updates.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or_default().to_lowercase())
    });

    log::info!(
        "gamebanana: {} of {} installed mod(s) for {game_id} have a newer upload",
        updates.len(),
        entries.len()
    );
    UPDATE_CACHE.set(&game_id, updates.clone());
    Ok(ok_with(json!({ "updates": updates })))
}

pub(super) async fn update_mod(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    if !mods::master_enabled(app) {
        return Ok(err_response("Turn on mod support first."));
    }
    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile).to_string();
    let Some(variant) = game_profiles::mod_variant(profile).map(str::to_string) else {
        return Ok(err_response(format!(
            "{} can't use mods.",
            game_profiles::display_name(profile)
        )));
    };
    let Some(local_id) = arg_str(args, 1).map(str::to_string) else {
        return Ok(err_response("No mod was specified."));
    };
    let Some(entry) = mods::gamebanana_entries(app, &game_id)
        .into_iter()
        .find(|e| e.mod_id == local_id)
    else {
        return Ok(err_response(
            "That mod did not come from GameBanana, or it is no longer installed.",
        ));
    };

    let chosen = args.get(2).and_then(Value::as_u64);
    let cached_target = || {
        UPDATE_CACHE
            .get(&game_id)
            .and_then(|list| list.into_iter().find(|u| u["modId"].as_str() == Some(&local_id)))
            .and_then(|u| u["fileId"].as_u64())
    };
    let file_id = match chosen.or_else(cached_target) {
        Some(id) => id,
        None => {
            let listing = match get_json(&format!("{API}/Mod/{}/DownloadPage", entry.gb_mod_id)).await {
                Ok(listing) => listing,
                Err(e) => return Ok(err_response(e)),
            };
            match update_target(&listing, entry.gb_file_id).and_then(|f| f.get("_idRow")?.as_u64()) {
                Some(id) => id,
                None => return Ok(err_response(format!("{} is already up to date.", entry.name))),
            }
        }
    };
    if file_id == entry.gb_file_id {
        return Ok(err_response(format!("{} is already up to date.", entry.name)));
    }

    let label = entry.name.clone();
    let publish = |message: &str, percent: f64| mods::publish_progress(app, &game_id, message, percent);
    let fetched = match fetch_gb_file(app, &game_id, entry.gb_mod_id, file_id, &label, None).await {
        Ok(fetched) => fetched,
        Err(e) if e == fs_util::CANCELLED_MSG => return Ok(cancelled_response()),
        Err(e) => return Ok(err_response(e)),
    };
    if fetched.cancel_requested(app, &game_id) {
        return Ok(cancelled_response());
    }

    let updating = format!("Updating {label}…");
    publish(&updating, INSTALL_PERCENT);
    let thumbnail = preview_url(&fetched.listing).or_else(|| entry.thumbnail_url.clone());
    let source = gb_source(&fetched, entry.gb_mod_id, file_id, thumbnail);
    let result = mods::replace_mod(
        app,
        &game_id,
        &variant,
        &entry.folder,
        &fetched.archive,
        source,
        Some((&updating, INSTALL_PERCENT, 99.0)),
    )
    .await;
    let _ = std::fs::remove_dir_all(&fetched.staging);

    match result {
        Ok(folder) => {
            mods::finish_progress(app, &game_id, false);
            if let Some(mut list) = UPDATE_CACHE.get(&game_id) {
                list.retain(|u| u["modId"].as_str() != Some(&local_id));
                UPDATE_CACHE.set(&game_id, list);
            }
            mods::notify_mods_changed(app, &game_id);
            Ok(ok_with(json!({ "folderName": folder, "modId": local_id })))
        }
        Err(e) => {
            mods::finish_progress(app, &game_id, true);
            Ok(err_response(e))
        }
    }
}

// ------------ Archive Downloads ------------
// The actual file download, with a size cap and a quick sanity check on the archive.
const OVERSIZED_DOWNLOAD: &str =
    "GameBanana sent more data than the file size it announced, so the download was stopped.";

const TOO_LARGE_DOWNLOAD: &str =
    "GameBanana sent a file larger than 8 GB, so the download was stopped.";

const MAX_ARCHIVE_BYTES: u64 = 8 << 30;

fn download_refusal(received: u64, announced: u64) -> Option<&'static str> {
    if announced > MAX_ARCHIVE_BYTES {
        Some(TOO_LARGE_DOWNLOAD)
    } else if announced > 0 {
        (received > announced).then_some(OVERSIZED_DOWNLOAD)
    } else {
        (received > MAX_ARCHIVE_BYTES).then_some(TOO_LARGE_DOWNLOAD)
    }
}

async fn download(
    url: &str,
    destination: &std::path::Path,
    listed_size: u64,
    cancel: &Arc<AtomicBool>,
    mut on_percent: impl FnMut(f64),
) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        return Err(fs_util::CANCELLED_MSG.to_string());
    }
    let job = http::ToolJob::new(Arc::clone(cancel));
    let refusal = |received: u64, announced: u64| {
        let announced = if announced > 0 { announced } else { listed_size };
        let refused = download_refusal(received, announced)?;
        log::warn!("gamebanana: {url} is refused ({received} received, {announced} announced): {refused}");
        Some(refused.to_string())
    };
    peebify_transfer::download_stream(
        &http::shared_transfer(),
        &job,
        url,
        destination,
        &refusal,
        &mut |written, announced| {
            let total = if announced > 0 { announced } else { listed_size };
            if total > 0 {
                on_percent((written as f64 / total as f64) * 100.0);
            }
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| if e == peebify_transfer::files::CANCELLED { fs_util::CANCELLED_MSG.to_string() } else { e })
}

fn archive_sanity_check(path: &std::path::Path) -> Result<(), String> {
    use std::io::Read;

    let mut head = [0u8; 512];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| format!("Could not read the downloaded file: {e}"))?;
    if read == 0 {
        return Err("The downloaded file was empty.".to_string());
    }
    let text = String::from_utf8_lossy(&head[..read]);
    let start = text.trim_start().to_ascii_lowercase();
    if start.starts_with("<!doctype") || start.starts_with("<html") {
        return Err(
            "GameBanana sent back a web page instead of the mod file. Try again in a moment."
                .to_string(),
        );
    }
    Ok(())
}

fn archive_file_name(raw: &str) -> String {
    let (stem, extension) = match raw.rsplit_once('.') {
        Some((stem, extension)) if !stem.trim().is_empty() => {
            (stem, fs_util::sanitize_folder_name(extension))
        }
        _ => (raw, String::new()),
    };
    let stem = fs_util::sanitize_folder_name(stem);
    let stem = if stem.is_empty() { "mod".to_string() } else { stem };
    if extension.is_empty() {
        stem
    } else {
        format!("{stem}.{extension}")
    }
}

// ------------ GameBanana Tests ------------
// Covers feed paging, in flight installs, update picking and download limits.
#[cfg(test)]
mod tests {
    use super::InFlight;

    #[test]
    fn in_flight_refuses_duplicate_until_released() {
        let first = InFlight::claim("test-a", 9_000_001, 7, None).expect("first claim");
        assert!(InFlight::claim("test-a", 9_000_001, 7, None).is_none());
        let other = InFlight::claim("test-a", 9_000_001, 8, None).expect("different file");
        drop(first);
        assert!(InFlight::claim("test-a", 9_000_001, 7, None).is_some());
        drop(other);
    }

    #[test]
    fn cancel_stops_only_its_own_download() {
        use super::cancel_fetches;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let restore_flag = Arc::new(AtomicBool::new(false));
        let restore =
            InFlight::claim("test-b", 9_000_002, 1, Some(restore_flag.clone())).expect("restore");
        let first = InFlight::claim("test-b", 9_000_002, 2, None).expect("first");
        let second = InFlight::claim("test-b", 9_000_002, 3, None).expect("second");
        let elsewhere = InFlight::claim("test-c", 9_000_002, 4, None).expect("other game");

        assert_eq!(cancel_fetches("test-b", Some((9_000_002, 2))), 1);
        assert!(first.cancel.load(Ordering::SeqCst));
        assert!(!second.cancel.load(Ordering::SeqCst));

        let third = InFlight::claim("test-b", 9_000_002, 5, None).expect("third");
        assert!(!third.cancel.load(Ordering::SeqCst));
        assert!(first.cancel.load(Ordering::SeqCst));

        assert_eq!(cancel_fetches("test-b", None), 3);
        assert!(second.cancel.load(Ordering::SeqCst));
        assert!(!restore_flag.load(Ordering::SeqCst));
        assert!(!elsewhere.cancel.load(Ordering::SeqCst));

        restore_flag.store(true, Ordering::SeqCst);
        assert!(restore.cancel.load(Ordering::SeqCst));
        drop((restore, first, second, third, elsewhere));
    }

    #[test]
    fn downloads_stop_past_the_announced_size_or_the_hard_cap() {
        use super::{download_refusal, MAX_ARCHIVE_BYTES, OVERSIZED_DOWNLOAD, TOO_LARGE_DOWNLOAD};
        assert_eq!(download_refusal(0, 1_000), None);
        assert_eq!(download_refusal(1_000, 1_000), None);
        assert_eq!(download_refusal(1_001, 1_000), Some(OVERSIZED_DOWNLOAD));
        assert_eq!(download_refusal(0, 0), None);
        assert_eq!(download_refusal(MAX_ARCHIVE_BYTES, 0), None);
        assert_eq!(download_refusal(MAX_ARCHIVE_BYTES + 1, 0), Some(TOO_LARGE_DOWNLOAD));
        assert_eq!(download_refusal(0, MAX_ARCHIVE_BYTES + 1), Some(TOO_LARGE_DOWNLOAD));
    }

    #[test]
    fn archive_names_keep_their_extension_and_avoid_devices() {
        use super::archive_file_name;
        assert_eq!(archive_file_name("nul.zip"), "nul_.zip");
        assert_eq!(archive_file_name("Keqing Summer.7z"), "Keqing Summer.7z");
        assert_eq!(archive_file_name("a:b.rar"), "a_b.rar");
        assert_eq!(archive_file_name(""), "mod");
        assert_eq!(archive_file_name(".zip"), ".zip");
        assert!(!archive_file_name("../x.zip").contains(['/', '\\']));
    }

    #[test]
    fn descriptions_decode_entities_and_keep_paragraphs() {
        use super::html_to_text;
        assert_eq!(
            html_to_text("<p>Don&#039;t &ldquo;touch&rdquo; &amp;lt;this&gt;&hellip;</p><p>Second&#x21;</p>"),
            "Don't \u{201C}touch\u{201D} &lt;this>\u{2026}\n\nSecond!"
        );
        assert_eq!(html_to_text("a<br>b<br><br><br>c"), "a\nb\n\nc");
        assert_eq!(html_to_text("<ul><li>one</li><li>two</li></ul>"), "one\ntwo");
        assert_eq!(html_to_text("R&D &unknown; & &#0; 5 &lt 6"), "R&D &unknown; & &#0; 5 &lt 6");
        assert_eq!(html_to_text("<script>x</script>&nbsp;hi&#160;there"), "hi there");
    }

    fn file(id: u64, name: &str, added: u64) -> serde_json::Value {
        serde_json::json!({
            "_idRow": id,
            "_sFile": name,
            "_tsDateAdded": added,
            "_sAvResult": "clean",
        })
    }

    fn target_id(listing: &serde_json::Value, installed: u64) -> Option<u64> {
        super::update_target(listing, installed)
            .and_then(|f| f.get("_idRow"))
            .and_then(serde_json::Value::as_u64)
    }

    #[test]
    fn file_family_ignores_versions_and_extension() {
        assert_eq!(super::file_family("Variant_A_v1.2.zip"), "variant a");
        assert_eq!(super::file_family("variant a V1.3.7z"), "variant a");
        assert_ne!(super::file_family("Variant_A.zip"), super::file_family("Variant_B.zip"));
    }

    #[test]
    fn sibling_variant_is_not_an_update() {
        let listing = serde_json::json!({
            "_aFiles": [file(1, "Variant A.zip", 100), file(2, "Variant B.zip", 105)],
        });
        assert_eq!(target_id(&listing, 1), None);
    }

    #[test]
    fn newer_upload_of_the_same_file_is_an_update() {
        let listing = serde_json::json!({
            "_aFiles": [
                file(1, "Variant A v1.zip", 100),
                file(2, "Variant B v1.zip", 105),
                file(3, "Variant A v2.zip", 200),
            ],
        });
        assert_eq!(target_id(&listing, 1), Some(3));
        assert_eq!(target_id(&listing, 3), None);
    }

    #[test]
    fn replaced_file_falls_back_to_newest_clean() {
        let listing = serde_json::json!({
            "_aFiles": [file(4, "Anything.zip", 300), file(5, "Other.zip", 250)],
        });
        assert_eq!(target_id(&listing, 1), Some(4));
    }

    fn working(clean: usize, total: Option<u64>, raw_seen: u64, exhausted: bool) -> super::FeedWorkingSet {
        super::FeedWorkingSet {
            clean: std::sync::Arc::new(vec![serde_json::Value::Null; clean]),
            raw_seen,
            next_page: 9,
            total,
            exhausted,
            started: std::time::Instant::now(),
        }
    }

    #[test]
    fn exhausted_feed_reports_only_reachable_pages() {
        let (pages, more) = super::feed_paging(&working(340, Some(5000), 400, true), 22, 16);
        assert_eq!(pages, 22);
        assert!(!more);
        let (_, more) = super::feed_paging(&working(340, Some(5000), 400, true), 21, 16);
        assert!(more);
    }

    #[test]
    fn open_feed_always_offers_a_next_page() {
        let (pages, more) = super::feed_paging(&working(340, Some(5000), 400, false), 21, 16);
        assert!(more);
        assert!(pages > 21);
    }

    #[test]
    fn only_definitive_thumbnail_answers_are_recorded() {
        use super::{sort_thumbnail_batch, thumbnail_answers};
        use std::collections::{HashMap, HashSet};

        let preview = serde_json::json!({
            "_idRow": 1,
            "_aPreviewMedia": { "_aImages": [{ "_sBaseUrl": "https://img/", "_sFile530": "a.jpg" }] },
        });
        let bare = serde_json::json!({ "_idRow": 2, "_aPreviewMedia": [] });
        let mut records = HashMap::new();
        let mut answered = HashSet::new();
        let mut refused = Vec::new();

        let found = HashMap::from([(1, preview), (2, bare)]);
        sort_thumbnail_batch(&[1, 2, 3], Ok(found), &mut records, &mut answered, &mut refused);
        let bad = || Err("GameBanana returned HTTP 400".to_string());
        sort_thumbnail_batch(&[4, 5], bad(), &mut records, &mut answered, &mut refused);
        sort_thumbnail_batch(&[4], bad(), &mut records, &mut answered, &mut refused);
        let down = Err("GameBanana returned HTTP 503".to_string());
        sort_thumbnail_batch(&[6], down, &mut records, &mut answered, &mut refused);
        assert_eq!(refused, vec![vec![4, 5]]);

        let missing = (1..=6).map(|id| (format!("m{id}"), id)).collect();
        let answers = thumbnail_answers(missing, &records, &answered);
        assert_eq!(
            answers,
            vec![
                ("m1".to_string(), Some("https://img/a.jpg".to_string())),
                ("m2".to_string(), None),
                ("m3".to_string(), None),
                ("m4".to_string(), None),
            ]
        );
    }
}
