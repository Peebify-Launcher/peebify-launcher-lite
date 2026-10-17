// ------------ HoYoverse (Sophon) ------------
// Hoyoverse's own download system, used for Genshin, Star Rail, Zenless and Honkai 3rd. A build is made of categories
// (game files, extra content, voice packs), each a store of zstd chunks with a protobuf list of the files they make up.
// This reads builds and their lists into the manifest the engines install, repair and verify from, and sizes the preview.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::AppHandle;

use super::engine::{merge_parts, JobKind, Preview};
use super::fs_util::safe_join;
use super::game_manager::local_game_version_at;
use super::manifest::{Chunk, Cx, Entry, Hash, Manifest, Policy, Publisher, Source, Store, MAX_CHUNK_BYTES};
use super::validator::{Record, Triage};
use super::{engine, game_profiles, http};

const BUILD_URL: &str = "https://sg-public-api.hoyoverse.com/downloader/sophon_chunk/api/getBuild";
const HYP_API_URL: &str = "https://sg-hyp-api.hoyoverse.com/hyp/hyp-connect/api";
const HYP_LAUNCHER_ID: &str = "VYTpXlbWo8";
const HYP_LANG: &str = "en-us";

pub const CATEGORY_GAME: &str = "game";
pub const HOYO_AUDIO_LANGUAGE: &str = "en-us";

const CONTROL_MAX_RETRIES: u32 = 3;
const CONTROL_RETRY_BASE_MS: u64 = 1000;

const BRANCHES_CACHE_KEY: &str = "branches";
static BRANCHES_CACHE: http::TtlCache<Value> =
    http::TtlCache::new(std::time::Duration::from_secs(120), 1);

// ------------ Manifest Parsing ------------
// Sophon manifests are protobuf. This is a tiny hand-written reader for them, so we do
// not need a protobuf dependency, plus the file and chunk types it produces.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut out: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos).ok_or("protobuf: truncated varint")?;
            self.pos += 1;
            out |= u64::from(b & 0x7f)
                .checked_shl(shift)
                .ok_or("protobuf: varint too long")?;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift > 63 {
                return Err("protobuf: varint overflow".into());
            }
        }
    }

    fn slice(&mut self) -> Result<&'a [u8], String> {
        let len = self.varint()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .filter(|e| *e <= self.buf.len())
            .ok_or("protobuf: length-delimited field overruns buffer")?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn string(&mut self) -> Result<String, String> {
        Ok(String::from_utf8_lossy(self.slice()?).into_owned())
    }

    fn skip(&mut self, wire: u64) -> Result<(), String> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.pos = self.pos.saturating_add(8),
            2 => {
                self.slice()?;
            }
            5 => self.pos = self.pos.saturating_add(4),
            other => return Err(format!("protobuf: unsupported wire type {other}")),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Asset {
    pub name: String,
    pub chunks: Vec<Chunk>,
    pub asset_type: i32,
    pub size: u64,
    pub md5: String,
}

impl Asset {
    pub fn is_dir(&self) -> bool {
        self.chunks.is_empty() && self.size == 0 && (self.asset_type != 0 || self.md5.is_empty())
    }
}

fn parse_chunk(buf: &[u8]) -> Result<Chunk, String> {
    let mut r = Reader::new(buf);
    let mut c = Chunk::default();
    while !r.eof() {
        let key = r.varint()?;
        match (key >> 3, key & 7) {
            (1, 2) => c.name = r.string()?.into(),
            (2, 2) => {
                let _ = hex::decode_to_slice(r.string()?.trim(), &mut c.md5);
            }
            (3, 0) => c.offset = r.varint()?,
            (4, 0) => c.compressed_size = r.varint()?,
            (5, 0) => c.size = r.varint()?,
            (_, w) => r.skip(w)?,
        }
    }
    Ok(c)
}

fn parse_asset(buf: &[u8]) -> Result<Asset, String> {
    let mut r = Reader::new(buf);
    let mut a = Asset::default();
    while !r.eof() {
        let key = r.varint()?;
        match (key >> 3, key & 7) {
            (1, 2) => a.name = r.string()?,
            (2, 2) => a.chunks.push(parse_chunk(r.slice()?)?),
            (3, 0) => a.asset_type = r.varint()? as i32,
            (4, 0) => a.size = r.varint()?,
            (5, 2) => a.md5 = r.string()?,
            (_, w) => r.skip(w)?,
        }
    }
    Ok(a)
}

pub fn parse_manifest(buf: &[u8]) -> Result<Vec<Asset>, String> {
    let mut r = Reader::new(buf);
    let mut assets = Vec::new();
    while !r.eof() {
        let key = r.varint()?;
        match (key >> 3, key & 7) {
            (1, 2) => assets.push(parse_asset(r.slice()?)?),
            (_, w) => r.skip(w)?,
        }
    }
    Ok(assets)
}

pub fn validate_asset(asset: &Asset) -> Result<(), String> {
    let mut expected = 0u64;
    for c in &asset.chunks {
        if c.size > MAX_CHUNK_BYTES {
            return Err(format!(
                "{}: chunk {} claims {} bytes, more than the {MAX_CHUNK_BYTES} byte limit",
                asset.name, c.name, c.size
            ));
        }
        if c.offset != expected {
            return Err(format!(
                "{}: chunk {} starts at {} but {} was expected",
                asset.name, c.name, c.offset, expected
            ));
        }
        expected += c.size;
    }
    if expected != asset.size {
        return Err(format!(
            "{}: chunks cover {expected} bytes but the file is {}",
            asset.name, asset.size
        ));
    }
    Ok(())
}

// ------------ Builds And Branches ------------
// Asks the Hoyoverse API which build of a game is current, and which categories (game
// files, each voice language) it is made of, along with the credentials to fetch them.
#[derive(Debug, Clone)]
pub struct Category {
    pub matching_field: String,
    pub manifest_id: String,
    pub manifest_url: String,
    pub chunk_prefix: String,
    pub chunk_compressed: bool,
    pub manifest_compressed: bool,
    pub total_bytes: u64,
    pub compressed_bytes: u64,
    pub file_count: u64,
}

#[derive(Debug, Clone)]
pub struct Build {
    pub tag: String,
    pub categories: Vec<Category>,
}

impl Build {
    pub fn category(&self, matching_field: &str) -> Option<&Category> {
        self.categories
            .iter()
            .find(|c| c.matching_field == matching_field)
    }
}

pub fn is_language_field(field: &str) -> bool {
    let f = field.strip_prefix("mini-").unwrap_or(field);
    matches!(f.as_bytes(), [a, b, b'-', c, d] if [a, b, c, d].iter().all(|x| x.is_ascii_lowercase()))
}

pub fn install_categories<'a>(build: &'a Build, audio_language: &str) -> Vec<&'a Category> {
    let mut out: Vec<&Category> = build
        .categories
        .iter()
        .filter(|c| !is_language_field(&c.matching_field) && !c.matching_field.starts_with("mini-"))
        .collect();
    out.sort_by_key(|c| c.matching_field != CATEGORY_GAME);
    if let Some(audio) = build.category(audio_language) {
        out.push(audio);
    }
    out
}

pub fn category_label(matching_field: &str) -> String {
    match matching_field {
        CATEGORY_GAME => "game files".to_string(),
        "asb" => "asset bundles".to_string(),
        f if f.chars().all(|c| c.is_ascii_digit()) => "extra content".to_string(),
        f if is_language_field(f) => format!("{} audio", f.strip_prefix("mini-").unwrap_or(f)),
        f => f.to_string(),
    }
}

fn join_url(prefix: &str, name: &str, suffix: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    format!("{prefix}/{name}{suffix}")
}

fn num(v: &Value) -> u64 {
    super::manifest::json_u64(Some(v)).unwrap_or(0)
}

pub fn parse_build(body: &Value) -> Result<Build, String> {
    let data = body.get("data").ok_or_else(|| {
        format!(
            "sophon getBuild failed: {}",
            body.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no data")
        )
    })?;

    let categories = data
        .get("manifests")
        .and_then(|v| v.as_array())
        .ok_or("sophon getBuild returned no manifests")?
        .iter()
        .filter_map(|m| {
            let manifest = m.get("manifest")?;
            let id = manifest.get("id")?.as_str()?.to_string();
            let md = m.get("manifest_download")?;
            let cd = m.get("chunk_download")?;
            let stats = m.get("stats");
            Some(Category {
                matching_field: m.get("matching_field")?.as_str()?.to_string(),
                manifest_url: join_url(
                    md.get("url_prefix")?.as_str()?,
                    &id,
                    md.get("url_suffix").and_then(|v| v.as_str()).unwrap_or(""),
                ),
                manifest_id: id,
                chunk_prefix: cd.get("url_prefix")?.as_str()?.to_string(),
                chunk_compressed: cd.get("compression").map(num).unwrap_or(0) != 0,
                manifest_compressed: md.get("compression").map(num).unwrap_or(0) != 0,
                total_bytes: stats
                    .and_then(|s| s.get("uncompressed_size"))
                    .map(num)
                    .unwrap_or(0),
                compressed_bytes: stats
                    .and_then(|s| s.get("compressed_size"))
                    .map(num)
                    .unwrap_or(0),
                file_count: stats
                    .and_then(|s| s.get("file_count"))
                    .map(num)
                    .unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    if categories.is_empty() {
        return Err("sophon getBuild returned no usable manifests".into());
    }

    Ok(Build {
        tag: data
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        categories,
    })
}

pub const BRANCH_MAIN: &str = "main";
const BRANCH_PRE_DOWNLOAD: &str = "pre_download";

static PRE_DOWNLOAD_LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, PartialEq)]
pub struct BranchAuth {
    pub package_id: String,
    pub password: String,
    pub tag: String,
    pub branch: String,
}

fn find_branch_entry<'a>(
    body: &'a Value,
    biz: &str,
    game_id: Option<&str>,
) -> Result<Option<&'a Value>, String> {
    let branches = body
        .get("data")
        .and_then(|d| d.get("game_branches"))
        .and_then(|v| v.as_array())
        .ok_or("sophon getGameBranches returned no branches")?;

    Ok(game_id
        .and_then(|id| {
            branches
                .iter()
                .find(|b| b.pointer("/game/id").and_then(|v| v.as_str()) == Some(id))
        })
        .or_else(|| {
            branches
                .iter()
                .find(|b| b.pointer("/game/biz").and_then(|v| v.as_str()) == Some(biz))
        }))
}

pub fn parse_branch(
    body: &Value,
    biz: &str,
    game_id: Option<&str>,
    key: &str,
) -> Result<BranchAuth, String> {
    let entry = find_branch_entry(body, biz, game_id)?
        .and_then(|b| b.get(key))
        .filter(|v| v.is_object())
        .ok_or_else(|| format!("sophon: no {key} branch for biz {biz}"))?;

    let field = |k: &str| {
        entry
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    Ok(BranchAuth {
        package_id: field("package_id")
            .ok_or_else(|| format!("sophon: {key} branch for {biz} has no package_id"))?,
        password: field("password")
            .ok_or_else(|| format!("sophon: {key} branch for {biz} has no password"))?,
        tag: field("tag").unwrap_or_default(),
        branch: field("branch")
            .or_else(|| (key == BRANCH_MAIN).then(|| BRANCH_MAIN.to_string()))
            .ok_or_else(|| format!("sophon: {key} branch for {biz} has no branch name"))?,
    })
}

fn pre_download_entry<'a>(body: &'a Value, biz: &str, game_id: Option<&str>) -> Option<&'a Value> {
    find_branch_entry(body, biz, game_id)
        .ok()
        .flatten()
        .and_then(|b| b.get(BRANCH_PRE_DOWNLOAD))
        .filter(|v| v.is_object())
}

fn pre_download_log_key(body: &Value, biz: &str, game_id: Option<&str>) -> Option<(String, String)> {
    let entry = pre_download_entry(body, biz, game_id)?;
    Some(match parse_branch(body, biz, game_id, BRANCH_PRE_DOWNLOAD) {
        Ok(auth) => {
            let tag = if auth.tag.is_empty() {
                "an untagged build".to_string()
            } else {
                auth.tag
            };
            (
                format!("{biz}:{}:{tag}", auth.branch),
                format!("sophon: {biz} pre-download is open for {tag} on branch {}", auth.branch),
            )
        }
        Err(e) => {
            let fields: Vec<&str> = entry
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            (
                format!("{biz}:unreadable"),
                format!(
                    "sophon: {biz} pre-download entry could not be read ({e}); fields: {}",
                    fields.join(", ")
                ),
            )
        }
    })
}

fn note_pre_download(body: &Value, biz: &str, game_id: Option<&str>) {
    let Some((key, message)) = pre_download_log_key(body, biz, game_id) else {
        return;
    };
    let mut logged = PRE_DOWNLOAD_LOGGED.lock();
    if logged.contains(&key) {
        return;
    }
    logged.push(key);
    drop(logged);
    log::info!("{message}");
}

pub(super) fn build_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let mut url = url::Url::parse(&format!("{HYP_API_URL}/{endpoint}"))
        .expect("HYP_API_URL + endpoint is always a valid URL");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("launcher_id", HYP_LAUNCHER_ID);
        query.append_pair("language", HYP_LANG);
        for (k, v) in params {
            query.append_pair(k, v);
        }
    }
    url.to_string()
}

async fn fetch_branches(allow_cached: bool) -> Result<Value, String> {
    if allow_cached {
        if let Some(body) = BRANCHES_CACHE.get(BRANCHES_CACHE_KEY) {
            return Ok(body);
        }
    }
    let url = build_url("getGameBranches", &[]);
    let body = http::with_retry(
        || http::get_json(&url),
        CONTROL_MAX_RETRIES,
        CONTROL_RETRY_BASE_MS,
        "sophon getGameBranches",
    )
    .await?;
    BRANCHES_CACHE.set(BRANCHES_CACHE_KEY, body.clone());
    Ok(body)
}

async fn branch_auth_for_profile(profile: &Value, allow_cached: bool) -> Result<BranchAuth, String> {
    let biz = profile
        .get("hoyoBiz")
        .and_then(|v| v.as_str())
        .ok_or("no hoyoBiz for sophon profile")?;
    let game_id = profile.get("hoyoGameId").and_then(|v| v.as_str());
    let body = fetch_branches(allow_cached).await?;
    note_pre_download(&body, biz, game_id);
    parse_branch(&body, biz, game_id, BRANCH_MAIN)
}

pub async fn fetch_branch_auth_for_profile(profile: &Value) -> Result<BranchAuth, String> {
    branch_auth_for_profile(profile, false).await
}

pub async fn cached_branch_auth_for_profile(profile: &Value) -> Result<BranchAuth, String> {
    branch_auth_for_profile(profile, true).await
}

pub async fn fetch_build(auth: &BranchAuth) -> Result<Build, String> {
    let url = format!(
        "{BUILD_URL}?branch={}&package_id={}&password={}",
        auth.branch, auth.package_id, auth.password
    );
    let body = http::with_retry(
        || http::get_json(&url),
        CONTROL_MAX_RETRIES,
        CONTROL_RETRY_BASE_MS,
        "sophon getBuild",
    )
    .await?;
    parse_build(&body)
}

const MAX_MANIFEST_BYTES: u64 = 1 << 30;

// ------------ Manifest Download ------------
// Downloads and decompresses each category's manifest, with a size cap so a bad
// response cannot eat all the memory.
fn decode_capped(bytes: &[u8], limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    zstd::stream::read::Decoder::new(bytes)?
        .take(limit + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > limit {
        return Err(std::io::Error::other(format!(
            "it unpacks to more than {limit} bytes"
        )));
    }
    Ok(out)
}

fn decode_manifest(category: &Category, bytes: &[u8]) -> Result<Vec<Asset>, String> {
    let decoded;
    let raw = if category.manifest_compressed {
        decoded = decode_capped(bytes, MAX_MANIFEST_BYTES).map_err(|e| {
            format!(
                "sophon: manifest {} is not valid zstd: {e}",
                category.manifest_id
            )
        })?;
        &decoded[..]
    } else {
        bytes
    };
    let assets = parse_manifest(raw)?;
    for asset in &assets {
        validate_asset(asset).map_err(|e| {
            format!(
                "sophon: manifest {} is malformed: {e}",
                category.manifest_id
            )
        })?;
    }
    log_empty_assets(&category.manifest_id, &assets);
    Ok(assets)
}

fn log_empty_assets(manifest_id: &str, assets: &[Asset]) {
    let mut histogram: std::collections::BTreeMap<(i32, bool), usize> =
        std::collections::BTreeMap::new();
    for asset in assets.iter().filter(|a| a.chunks.is_empty() && a.size == 0) {
        *histogram
            .entry((asset.asset_type, asset.md5.is_empty()))
            .or_default() += 1;
    }
    if histogram.is_empty() {
        return;
    }
    let summary: Vec<String> = histogram
        .iter()
        .map(|((kind, no_md5), count)| {
            let md5 = if *no_md5 { "empty" } else { "set" };
            format!("type {kind} md5 {md5}: {count}")
        })
        .collect();
    log::info!(
        "sophon: manifest {manifest_id} zero size assets by kind ({})",
        summary.join(", ")
    );
}

pub async fn fetch_manifest(category: &Category) -> Result<Vec<Asset>, String> {
    let cache_path = manifest_cache_path(&category.manifest_id);

    if let Some(path) = &cache_path {
        if let Ok(bytes) = tokio::fs::read(path).await {
            let cat = category.clone();
            let decoded = tauri::async_runtime::spawn_blocking(move || decode_manifest(&cat, &bytes))
                .await
                .map_err(|e| format!("sophon: manifest decode task failed: {e}"))?;
            match decoded {
                Ok(assets) => return Ok(assets),
                Err(e) => {
                    log::warn!(
                        "sophon: cached manifest {} is unreadable ({e}) — refetching",
                        category.manifest_id
                    );
                    let _ = tokio::fs::remove_file(path).await;
                }
            }
        }
    }

    let bytes = http::with_retry(
        || http::get_bytes(&category.manifest_url),
        CONTROL_MAX_RETRIES,
        CONTROL_RETRY_BASE_MS,
        &format!("sophon manifest {}", category.manifest_id),
    )
    .await?;
    let (cat, raw) = (category.clone(), bytes.clone());
    let assets = tauri::async_runtime::spawn_blocking(move || decode_manifest(&cat, &raw))
        .await
        .map_err(|e| format!("sophon: manifest decode task failed: {e}"))??;

    if let Some(path) = cache_path {
        let saved =
            tauri::async_runtime::spawn_blocking(move || super::fs_util::write_atomic(&path, &bytes))
                .await;
        if let Ok(Err(e)) = saved {
            log::warn!("sophon: could not cache manifest {}: {e}", category.manifest_id);
        }
    }
    Ok(assets)
}

pub async fn fetch_manifests(categories: &[&Category]) -> Result<Vec<Vec<Asset>>, String> {
    use futures::stream::{self, StreamExt, TryStreamExt};
    let owned: Vec<Category> = categories.iter().map(|c| (*c).clone()).collect();
    stream::iter(owned)
        .map(|c| async move { fetch_manifest(&c).await })
        .buffered(8)
        .try_collect()
        .await
}

// ------------ Installed State ------------
// The installed build and voice pack: from Peebify's install record, or from what HoYoPlay left and the files on disk.

fn installed_version(install_dir: &Path) -> Option<String> {
    Record::version_at(install_dir).or_else(|| {
        std::fs::read_to_string(install_dir.join("config.ini"))
            .ok()
            .and_then(|text| hoyoplay_game_version(&text))
    })
}

fn hoyoplay_game_version(ini: &str) -> Option<String> {
    super::fs_util::ini_entries(ini.trim_start_matches('﻿'))
        .filter(|(section, _, _)| section.is_empty() || section.eq_ignore_ascii_case("general"))
        .find(|(_, key, value)| key.eq_ignore_ascii_case("game_version") && !value.is_empty())
        .map(|(_, _, value)| value.to_string())
}

pub(super) async fn latest_tag(profile: &Value) -> Result<String, String> {
    let auth = cached_branch_auth_for_profile(profile).await?;
    if !auth.tag.is_empty() {
        return Ok(auth.tag);
    }
    Ok(fetch_build(&auth).await?.tag)
}

pub fn count_present_assets(install_dir: &Path, assets: &[Asset]) -> usize {
    assets
        .iter()
        .filter(|a| !a.is_dir())
        .filter(|a| {
            safe_join(install_dir, &a.name)
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.is_file() && m.len() == a.size)
        })
        .count()
}

pub fn pick_voice_language(counts: &[(String, usize, usize)]) -> Option<String> {
    let mut best: Option<&(String, usize, usize)> = None;
    for entry in counts {
        if entry.1 > 0 && best.is_none_or(|b| entry.1 > b.1) {
            best = Some(entry);
        }
    }
    best.map(|(lang, _, _)| lang.clone())
}

pub async fn detect_voice_language(
    install_dir: &Path,
    build: &Build,
    languages: &[&str],
) -> Result<Option<String>, String> {
    let categories: Vec<&Category> = languages
        .iter()
        .filter_map(|lang| build.category(lang))
        .collect();
    if categories.is_empty() {
        return Ok(None);
    }
    let manifests = fetch_manifests(&categories).await?;
    let fields: Vec<String> = categories.iter().map(|c| c.matching_field.clone()).collect();
    let dir = install_dir.to_path_buf();
    let counts = tauri::async_runtime::spawn_blocking(move || {
        fields
            .into_iter()
            .zip(&manifests)
            .map(|(field, assets)| {
                let total = assets.iter().filter(|a| !a.is_dir()).count();
                (field, count_present_assets(&dir, assets), total)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("voice pack detection panicked: {e}"))?;

    let significant: Vec<String> = counts
        .iter()
        .filter(|(_, present, total)| *present > 0 && *present * 10 >= *total)
        .map(|(lang, present, total)| format!("{lang} {present}/{total}"))
        .collect();
    if significant.len() > 1 {
        log::info!(
            "sophon: more than one voice pack is on disk in {} ({}); the one with the most files wins.",
            install_dir.display(),
            significant.join(", ")
        );
    }
    Ok(pick_voice_language(&counts))
}

static MANIFEST_CACHE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn set_manifest_cache_dir(dir: PathBuf) {
    *MANIFEST_CACHE_DIR.lock() = Some(dir);
}

fn manifest_cache_path(manifest_id: &str) -> Option<PathBuf> {
    if manifest_id.is_empty()
        || !manifest_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    MANIFEST_CACHE_DIR
        .lock()
        .as_ref()
        .map(|d| d.join(manifest_id))
}

pub fn sweep_manifest_cache(max_age: std::time::Duration) {
    let Some(dir) = MANIFEST_CACHE_DIR.lock().clone() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age > max_age)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ------------ Applied Voice Packs ------------
// The get-applied-voice-packs command: which voice languages an installed Hoyoverse game has.
pub(super) async fn get_applied_voice_packs(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let profile = match super::game_file_ops::known_profile_or_error(app, super::arg_str(args, 0)) {
        Ok(profile) => profile,
        Err(refusal) => return Ok(refusal),
    };
    if super::game_profiles::install_mode(profile) != Some("sophon") {
        return Ok(super::ok_with(json!({ "languages": Value::Null })));
    }
    let key = format!("games.{}.gamePath", super::game_profiles::profile_id(profile));
    let game_path = super::backend(app).config.get(key.as_str());
    let game_path = game_path.as_str().unwrap_or("").to_string();
    if game_path.is_empty() {
        return Ok(super::ok_with(json!({ "languages": Value::Null })));
    }
    let languages = tauri::async_runtime::spawn_blocking(move || {
        let record = Record::load(Path::new(&game_path));
        (!record.version.is_empty()).then(|| voice_languages(&record.groups))
    })
    .await
    .unwrap_or(None);
    Ok(super::ok_with(json!({ "languages": languages })))
}

// ------------ Install Preview ------------
// What an install of the current build downloads and installs, per category.
static BUILD_CACHE: http::TtlCache<Build> = http::TtlCache::new(std::time::Duration::from_secs(120), 4);

async fn cached_build(auth: &BranchAuth) -> Result<Build, String> {
    let key = format!("{}:{}:{}", auth.branch, auth.package_id, auth.tag);
    if let Some(build) = BUILD_CACHE.get(&key) {
        return Ok(build);
    }
    let build = fetch_build(auth).await?;
    BUILD_CACHE.set(&key, build.clone());
    Ok(build)
}

pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    let auth = cached_branch_auth_for_profile(profile).await?;
    let build = cached_build(&auth).await?;
    let audio_language = game_profiles::audio_language(game_profiles::profile_id(profile));
    let categories = install_categories(&build, &audio_language);

    let entries: Vec<(String, String, u64, u64)> = categories
        .iter()
        .map(|c| {
            (
                c.matching_field.clone(),
                category_label(&c.matching_field),
                c.total_bytes,
                c.file_count,
            )
        })
        .collect();

    Ok(Preview {
        version: build.tag.clone(),
        download_bytes: categories.iter().map(|c| c.compressed_bytes).sum(),
        install_bytes: categories.iter().map(|c| c.total_bytes).sum(),
        file_count: categories.iter().map(|c| c.file_count).sum(),
        approximate: false,
        parts: merge_parts(&entries),
    })
}

// ------------ Publisher ------------
// A build's manifest: each category is a chunk store, its files are chunked entries and its folders the manifest's.
// An install trusts the files the record has at the same checksum, and repair and verify follow the latest build.
pub(super) struct Sophon;

impl Publisher for Sophon {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let auth = fetch_branch_auth_for_profile(cx.profile).await?;
            let build = fetch_build(&auth).await?;
            let audio_language = match cx.purpose {
                JobKind::Install => game_profiles::resolve_audio_language(cx.app, cx.profile, cx.dir, Some(&build)).await,
                JobKind::Repair | JobKind::Verify => installed_audio_language(cx.app, cx.profile, cx.dir, &build).await,
            };
            let categories = install_categories(&build, &audio_language);
            if categories.is_empty() {
                return Err(format!("{}: sophon build has no usable categories.", game_profiles::display_name(cx.profile)));
            }
            let assets = fetch_manifests(&categories).await?;
            Ok(manifest_of(build.tag.clone(), &categories, assets))
        })
    }

    fn triage(&self, dir: &Path, files: &[Entry]) -> Triage {
        Triage { trusted: Record::load(dir).vouches(dir, files), ..Triage::default() }
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({ "version": latest_tag(profile).await? })) })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn installed_version(&self, dir: &Path) -> Option<String> {
        local_game_version_at(dir).or_else(|| installed_version(dir))
    }

    fn policy(&self, _dir: &Path) -> Policy {
        Policy { latest: true, checksums: true, records: true, nested: true, ..Policy::default() }
    }
}

fn manifest_of(version: String, categories: &[&Category], assets: Vec<Vec<Asset>>) -> Manifest {
    let mut manifest = Manifest { version, ..Manifest::default() };
    for (category, assets) in categories.iter().zip(assets) {
        let store = Arc::new(Store { prefix: category.chunk_prefix.as_str().into(), compressed: category.chunk_compressed });
        if let Some(origin) = engine::url_origin(&category.chunk_prefix).filter(|o| !manifest.mirrors.contains(o)) {
            manifest.mirrors.push(origin);
        }
        manifest.groups.push(category.matching_field.clone());
        for asset in assets {
            if asset.is_dir() {
                manifest.dirs.push(asset.name.into());
                continue;
            }
            manifest.entries.push(Entry {
                path: asset.name.into(),
                size: asset.size,
                hash: Hash::parse(&asset.md5),
                source: Source::Chunks { store: Arc::clone(&store), chunks: asset.chunks.into() },
                live: false,
            });
        }
    }
    manifest
}

// ------------ Voice Packs ------------
// The voice pack an install holds, which repair and verify check even when another one is picked for the next Apply.

pub(super) fn voice_languages(groups: &[String]) -> Vec<String> {
    groups.iter().filter(|g| !g.starts_with("mini-") && is_language_field(g)).cloned().collect()
}

fn installed_language(selected: String, groups: &[String]) -> String {
    let recorded = voice_languages(groups);
    if recorded.is_empty() || recorded.contains(&selected) {
        return selected;
    }
    recorded
        .into_iter()
        .find(|lang| game_profiles::VOICE_LANGUAGES.contains(&lang.as_str()))
        .unwrap_or(selected)
}

async fn installed_audio_language(app: &AppHandle, profile: &Value, game_path: &Path, build: &Build) -> String {
    let dir = game_path.to_path_buf();
    let groups = tauri::async_runtime::spawn_blocking(move || Record::load(&dir).groups).await.unwrap_or_default();
    let selected = game_profiles::resolve_audio_language(app, profile, game_path, Some(build)).await;
    let installed = installed_language(selected.clone(), &groups);
    if installed != selected {
        log::info!(
            "[voice] {}: checking the installed {installed} voice pack; {selected} is only downloaded on Apply.",
            game_profiles::profile_id(profile)
        );
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::{md5_hex, TempDir};

    #[test]
    fn manifests_that_unpack_past_the_limit_are_refused() {
        let packed = zstd::encode_all(&vec![0u8; 4096][..], 3).unwrap();
        assert_eq!(decode_capped(&packed, 4096).unwrap().len(), 4096);
        assert!(decode_capped(&packed, 4095).is_err());
        assert!(decode_capped(b"not zstd", 4096).is_err());
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_records_voice_languages_are_its_full_audio_categories() {
        assert_eq!(voice_languages(&strings(&["game", "asb", "ja-jp", "mini-en-us"])), strings(&["ja-jp"]));
        assert!(voice_languages(&strings(&["game", "1234"])).is_empty());
    }

    #[test]
    fn count_present_assets_needs_the_exact_size() {
        let dir = TempDir::new("count-present");
        dir.write("Audio/Japanese/a.pck", b"abcd");
        dir.write("Audio/Japanese/b.pck", b"ab");
        let sized = |name: &str, size: u64| Asset { name: name.to_string(), size, md5: "x".to_string(), ..Asset::default() };
        let assets = vec![
            sized("Audio/Japanese/a.pck", 4),
            sized("Audio/Japanese/b.pck", 4),
            sized("Audio/Japanese/c.pck", 4),
            Asset { name: "Audio/Japanese".to_string(), asset_type: 64, ..Asset::default() },
        ];
        assert_eq!(count_present_assets(&dir.0, &assets), 1);
    }

    #[test]
    fn pick_voice_language_takes_the_pack_with_the_most_files() {
        let counts = |list: &[(&str, usize)]| list.iter().map(|(l, n)| (l.to_string(), *n, 100)).collect::<Vec<_>>();
        assert_eq!(
            pick_voice_language(&counts(&[("en-us", 3), ("ja-jp", 90), ("ko-kr", 0)])),
            Some("ja-jp".to_string())
        );
        assert_eq!(pick_voice_language(&counts(&[("en-us", 40), ("ja-jp", 40)])), Some("en-us".to_string()));
        assert_eq!(pick_voice_language(&counts(&[("en-us", 0), ("ja-jp", 0)])), None);
        assert_eq!(pick_voice_language(&[]), None);
    }

    fn asset_of(name: &str, parts: &[&[u8]]) -> Asset {
        let mut offset = 0u64;
        let chunks = parts
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                let chunk = Chunk {
                    name: format!("chunk{i}").into(),
                    offset,
                    compressed_size: bytes.len() as u64,
                    size: bytes.len() as u64,
                    ..Chunk::default()
                };
                offset += bytes.len() as u64;
                chunk
            })
            .collect();
        Asset { name: name.to_string(), chunks, asset_type: 0, size: offset, md5: md5_hex(&parts.concat()) }
    }

    fn category(field: &str, prefix: &str) -> Category {
        Category {
            matching_field: field.to_string(),
            manifest_id: String::new(),
            manifest_url: String::new(),
            chunk_prefix: prefix.to_string(),
            chunk_compressed: true,
            manifest_compressed: false,
            total_bytes: 0,
            compressed_bytes: 0,
            file_count: 0,
        }
    }

    #[test]
    fn a_build_becomes_chunked_entries_sharing_one_store_per_category() {
        let game = category("game", "https://a.example.com/chunks");
        let voice = category("ja-jp", "https://a.example.com/audio");
        let folder = Asset { name: "GenshinImpact_Data/Empty".to_string(), asset_type: 64, ..Asset::default() };
        let assets = vec![
            vec![asset_of("a.blk", &[b"aaaa", b"bb"]), asset_of("b.blk", &[b"cc"]), folder],
            vec![asset_of("Audio/ja.pck", &[b"dd"])],
        ];
        let manifest = manifest_of("5.3.0".to_string(), &[&game, &voice], assets);
        assert_eq!(manifest.version, "5.3.0");
        assert_eq!(manifest.entries.iter().map(|e| &*e.path).collect::<Vec<_>>(), ["a.blk", "b.blk", "Audio/ja.pck"]);
        assert_eq!(manifest.dirs, vec![Box::<str>::from("GenshinImpact_Data/Empty")]);
        assert_eq!(manifest.mirrors, strings(&["https://a.example.com"]));
        assert_eq!(manifest.groups, strings(&["game", "ja-jp"]));
        let store = |e: &Entry| match &e.source {
            Source::Chunks { store, .. } => Arc::clone(store),
            _ => unreachable!(),
        };
        assert!(Arc::ptr_eq(&store(&manifest.entries[0]), &store(&manifest.entries[1])));
        assert_eq!(&*store(&manifest.entries[2]).prefix, "https://a.example.com/audio");
        let first = &manifest.entries[0];
        assert_eq!((first.size, first.fetch_size(), first.hash.expected()), (6, 6, md5_hex(b"aaaabb")));
    }

    #[test]
    fn a_missing_chunk_offset_reads_as_zero() {
        let chunk = parse_chunk(&[0x0a, 1, b'a', 0x28, 7]).unwrap();
        assert_eq!(&*chunk.name, "a");
        assert_eq!(chunk.offset, 0);
        assert_eq!(chunk.size, 7);
    }

    #[test]
    fn an_empty_file_is_not_a_directory() {
        let empty_file = Asset { name: "marker".to_string(), md5: md5_hex(b""), ..Asset::default() };
        assert!(!empty_file.is_dir());
        let dir = Asset { name: "folder".to_string(), ..Asset::default() };
        assert!(dir.is_dir());
        let typed_dir = Asset { name: "folder".to_string(), asset_type: 64, md5: md5_hex(b""), ..Asset::default() };
        assert!(typed_dir.is_dir());
    }

    #[test]
    fn an_oversized_chunk_is_rejected() {
        let mut asset = asset_of("Data/blob.blk", &[b"aaaa"]);
        asset.chunks[0].size = MAX_CHUNK_BYTES + 1;
        asset.size = MAX_CHUNK_BYTES + 1;
        assert!(validate_asset(&asset).unwrap_err().contains("byte limit"));
    }

    fn branches_body(pre_download: Value) -> Value {
        serde_json::json!({
            "data": {
                "game_branches": [
                    {
                        "game": { "id": "other", "biz": "nap_global" },
                        "main": { "package_id": "p0", "password": "w0", "tag": "1.0.0" },
                        "pre_download": null
                    },
                    {
                        "game": { "id": "gopR6Cufr3", "biz": "hk4e_global" },
                        "main": {
                            "package_id": "p1",
                            "branch": "main",
                            "password": "w1",
                            "tag": "5.0.0"
                        },
                        "pre_download": pre_download
                    }
                ]
            }
        })
    }

    #[test]
    fn the_main_branch_defaults_its_branch_name_to_main() {
        let body = branches_body(Value::Null);
        let auth = parse_branch(&body, "nap_global", None, BRANCH_MAIN).unwrap();
        assert_eq!(auth.branch, "main");
        assert_eq!(auth.package_id, "p0");
        let auth = parse_branch(&body, "hk4e_global", Some("gopR6Cufr3"), BRANCH_MAIN).unwrap();
        assert_eq!(auth.tag, "5.0.0");
        assert_eq!(auth.branch, "main");
    }

    #[test]
    fn the_pre_download_branch_carries_its_own_branch_name() {
        let body = branches_body(serde_json::json!({
            "package_id": "p2",
            "branch": "predownload",
            "password": "w2",
            "tag": "5.1.0"
        }));
        let auth = parse_branch(&body, "hk4e_global", Some("gopR6Cufr3"), BRANCH_PRE_DOWNLOAD).unwrap();
        assert_eq!(
            auth,
            BranchAuth {
                package_id: "p2".to_string(),
                password: "w2".to_string(),
                tag: "5.1.0".to_string(),
                branch: "predownload".to_string(),
            }
        );
        let (key, message) = pre_download_log_key(&body, "hk4e_global", Some("gopR6Cufr3")).unwrap();
        assert_eq!(key, "hk4e_global:predownload:5.1.0");
        assert!(message.contains("5.1.0"));
        assert!(!message.contains("w2"));
    }

    #[test]
    fn a_closed_pre_download_is_not_reported() {
        let body = branches_body(Value::Null);
        assert!(parse_branch(&body, "hk4e_global", None, BRANCH_PRE_DOWNLOAD).is_err());
        assert!(pre_download_log_key(&body, "hk4e_global", None).is_none());
        assert!(pre_download_log_key(&body, "nap_global", None).is_none());
    }

    #[test]
    fn a_pre_download_without_a_branch_name_is_reported_without_its_secrets() {
        let body = branches_body(serde_json::json!({
            "package_id": "p2",
            "password": "w2",
            "tag": "5.1.0"
        }));
        assert!(parse_branch(&body, "hk4e_global", None, BRANCH_PRE_DOWNLOAD).is_err());
        let (key, message) = pre_download_log_key(&body, "hk4e_global", None).unwrap();
        assert_eq!(key, "hk4e_global:unreadable");
        assert!(message.contains("package_id"));
        assert!(!message.contains("w2"));
    }

    #[test]
    fn verify_checks_the_installed_voice_pack_not_one_only_picked() {
        let installed = strings(&["game", "en-us"]);
        assert_eq!(installed_language("ja-jp".to_string(), &installed), "en-us");
        assert_eq!(installed_language("en-us".to_string(), &installed), "en-us");
        let both = strings(&["game", "en-us", "ja-jp"]);
        assert_eq!(installed_language("ja-jp".to_string(), &both), "ja-jp");
        assert_eq!(installed_language("ja-jp".to_string(), &[]), "ja-jp");
        assert_eq!(installed_language("ja-jp".to_string(), &strings(&["game"])), "ja-jp");
    }

    #[test]
    fn hoyoplay_version_comes_from_the_general_section() {
        let ini = "\u{feff}[general]\r\nchannel=1\r\ngame_version = 5.1.0 \r\n[other]\r\ngame_version=9.9.9\r\n";
        assert_eq!(hoyoplay_game_version(ini), Some("5.1.0".to_string()));
    }

    #[test]
    fn hoyoplay_version_ignores_empty_and_foreign_sections() {
        assert_eq!(hoyoplay_game_version("[general]\ngame_version=\n"), None);
        assert_eq!(hoyoplay_game_version("[launcher]\ngame_version=1.0\n"), None);
        assert_eq!(hoyoplay_game_version("GAME_VERSION=2.0"), Some("2.0".to_string()));
        assert_eq!(hoyoplay_game_version(""), None);
    }
}
