// ------------ Kuro Games ------------
// Wuthering Waves and Punishing: Gray Raven. Reads Kuro's launcher config (one live channel, or a set of
// resource-quality bundles), fetches the file lists from its CDNs, and tracks which quality is on disk.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use peebify_transfer::net::url_host;
use peebify_transfer::part::discard_part;
use peebify_transfer::{progress, Sealed};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::engine::{combine_url, resource_list_preview, JobCore, JobKind, Preview};
use super::manifest::{json_u64, Cx, Entry, Manifest, Policy, Publisher};
use super::validator::{write_game_config, GAME_CONFIG_FILE};
use super::{err_response, game_profiles, http, ok_with, BackendState};

pub(super) const LIVE_CHANNEL: &str = "default";
pub(super) const PACK_RECORD_DIR: &str = "launcherDownloadConfig";

// ------------ Publisher ------------
// An install fetches the selected quality; a repair and a verify check the quality that is installed. The packs of a
// quality are one exclusive group: once an install or switch puts it on disk, the files only other qualities use go.
// An install with no version file (a Steam copy) is checked by checksum on its first repair, which records it.
#[derive(Default)]
pub(super) struct Kuro {
    bundle: Mutex<Option<Bundle>>,
}

impl Publisher for Kuro {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let selected = selected_quality(cx.app, cx.profile);
            let quality = match cx.purpose {
                JobKind::Install => selected,
                JobKind::Repair | JobKind::Verify => {
                    installed_quality(cx.profile, cx.dir, selected.as_deref()).or(selected)
                }
            };
            let (manifest, bundle) = resolve(cx.profile, quality.as_deref()).await?;
            *self.bundle.lock() = bundle;
            Ok(manifest)
        })
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let url = profile.get("gameConfigUrl").and_then(Value::as_str).ok_or("no gameConfigUrl for profile")?;
            log::debug!("Fetching game config from: {url}");
            http::get_json(url).await
        })
    }

    fn preview<'a>(&'a self, profile: &'a Value, quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile, quality))
    }

    fn policy(&self, dir: &Path) -> Policy {
        let unrecorded = super::game_manager::local_game_version_at(dir).is_none();
        Policy { checksums: unrecorded, records: unrecorded, ..Policy::default() }
    }

    fn after_seal<'a>(
        &'a self,
        job: &'a Arc<JobCore>,
        dir: &'a Path,
        manifest: &'a Manifest,
        sealed: &'a Sealed,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let bundle = self.bundle.lock().clone();
            write_game_config(sealed, dir, &manifest.version, app_id(job.profile()), bundle.as_ref())?;
            if let (Some(bundle), JobKind::Install) = (&bundle, job.kind) {
                remove_other_qualities(job.profile(), dir, bundle, &manifest.entries).await;
            }
            Ok(true)
        })
    }
}

// ------------ Download Manifest ------------
// The file list, CDN mirrors and version of the live channel or of a quality's packs.
pub(super) async fn resolve(
    profile: &Value,
    quality: Option<&str>,
) -> Result<(Manifest, Option<Bundle>), String> {
    let config_url = profile
        .get("gameConfigUrl")
        .and_then(|v| v.as_str())
        .ok_or("no gameConfigUrl for profile")?;
    let name = game_profiles::display_name(profile);
    let game_config =
        http::with_retry(|| http::get_json(config_url), 3, 1000, "Kuro game config").await?;
    if is_bundle_config(&game_config) {
        let quality = quality
            .or_else(|| game_profiles::resource_quality_default(profile))
            .ok_or_else(|| format!("{name}: no resource quality is set for this game."))?;
        return resolve_bundle(name, &game_config, &bundle_name(quality)).await;
    }
    let channel = live_channel(&game_config)?;

    let cdns = cdn_urls(channel);
    if cdns.is_empty() {
        return Err("game config has no cdnList url".to_string());
    }
    let index_file = channel["config"]["indexFile"].as_str().unwrap_or("");
    if index_file.trim().is_empty() {
        return Err(format!(
            "{name}: the server returned a configuration without a file list."
        ));
    }
    let base_rel = channel["config"]["baseUrl"].as_str().unwrap_or("");

    let (primary, index) = fetch_index(name, &cdns, index_file).await?;
    let mirrors = mirrors_from(&cdns, primary, base_rel);
    let entries = index_entries(&index, &mirrors[0]);
    if entries.is_empty() {
        return Err(format!("{name}: the server returned an empty file list."));
    }
    let version = channel_version(channel);
    Ok((Manifest { version, entries, mirrors, ..Default::default() }, None))
}

async fn resolve_bundle(
    name: &str,
    game_config: &Value,
    bundle: &str,
) -> Result<(Manifest, Option<Bundle>), String> {
    let packs = bundle_packs(game_config, bundle).map_err(|e| format!("{name}: {e}."))?;
    let cdns = cdn_urls(game_config);
    if cdns.is_empty() {
        return Err("game config has no cdnList url".to_string());
    }

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut base_rel: Option<&str> = None;
    let mut mirrors: Vec<String> = Vec::new();
    let mut version = String::new();
    for pack_name in &packs {
        let pack = &game_config["resourcePacks"][pack_name];
        let index_file = pack["indexFile"].as_str().unwrap_or("").trim();
        if index_file.is_empty() {
            return Err(format!(
                "{name}: the {pack_name} pack of the {bundle} quality has no file list."
            ));
        }
        let pack_base = pack["baseUrl"].as_str().unwrap_or("");
        match base_rel {
            None => base_rel = Some(pack_base),
            Some(base) if base != pack_base => {
                return Err(format!(
                    "{name}: the {bundle} packs download from different folders, which Peebify can't handle yet."
                ));
            }
            Some(_) => {}
        }
        let (cdn, index) = fetch_index(name, &cdns, index_file).await?;
        if mirrors.is_empty() {
            mirrors = mirrors_from(&cdns, cdn, pack_base);
        }
        if version.is_empty() {
            version = channel_version(pack);
        }
        for entry in index_entries(&index, &mirrors[0]) {
            if seen.insert(crate::backend::fs_util::manifest_key(&entry.path)) {
                entries.push(entry);
            }
        }
    }
    if entries.is_empty() {
        return Err(format!("{name}: the server returned an empty file list."));
    }
    log::info!(
        "{name}: {bundle} quality is {} ({} files).",
        packs.join(" + "),
        entries.len()
    );
    let bundle = Bundle {
        name: bundle.to_string(),
        other_indexes: other_quality_indexes(game_config, &packs),
        packs,
        cdns,
    };
    Ok((Manifest { version, entries, mirrors, ..Default::default() }, Some(bundle)))
}

async fn fetch_index(
    name: &str,
    cdns: &[String],
    index_file: &str,
) -> Result<(usize, Value), String> {
    let mut last_error = String::new();
    for (i, cdn_url) in cdns.iter().enumerate() {
        let resource_list_url = combine_url(cdn_url, index_file);
        match http::with_retry(
            || http::get_json(&resource_list_url),
            3,
            1000,
            "Kuro index",
        )
        .await
        {
            Ok(index) => return Ok((i, index)),
            Err(e) => {
                if i + 1 < cdns.len() {
                    log::warn!(
                        "{name}: the file list from {} failed ({e}), trying the next CDN.",
                        url_host(cdn_url)
                    );
                }
                last_error = e;
            }
        }
    }
    Err(last_error)
}

fn mirrors_from(cdns: &[String], primary: usize, base_rel: &str) -> Vec<String> {
    std::iter::once(primary)
        .chain((0..cdns.len()).filter(|i| *i != primary))
        .map(|i| combine_url(&cdns[i], base_rel))
        .collect()
}

fn cdn_urls(channel: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in channel["cdnList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["url"].as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        if !out.iter().any(|seen| seen == url) {
            out.push(url.to_string());
        }
    }
    out
}

fn live_channel(game_config: &Value) -> Result<&Value, String> {
    game_config
        .get(LIVE_CHANNEL)
        .filter(|c| !c.is_null())
        .ok_or_else(|| format!("Could not find a '{LIVE_CHANNEL}' configuration."))
}

fn channel_version(channel: &Value) -> String {
    channel.get("version").and_then(super::fs_util::json_version).unwrap_or_default()
}

fn index_entries(index: &Value, base: &str) -> Vec<Entry> {
    index
        .get("resource")
        .or_else(|| index.get("resources"))
        .and_then(|v| v.as_array())
        .map(|list| list.iter().map(|v| Entry::from_json(v, base).live()).collect())
        .unwrap_or_default()
}

pub(super) fn config_size_at(config: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| json_u64(config.get(*key)))
        .filter(|n| *n > 0)
}

pub(super) struct Headline {
    pub(super) download_bytes: Option<u64>,
    pub(super) install_bytes: Option<u64>,
    pub(super) version: String,
}

pub(super) async fn headline(
    profile: &Value,
    quality: Option<&str>,
) -> Result<Headline, String> {
    let config_url = profile
        .get("gameConfigUrl")
        .and_then(|v| v.as_str())
        .ok_or("no gameConfigUrl for profile")?;
    let game_config = http::get_json(config_url).await?;
    if is_bundle_config(&game_config) {
        let bytes = quality
            .or_else(|| game_profiles::resource_quality_default(profile))
            .and_then(|q| bundle_bytes(&game_config, &bundle_name(q)));
        return Ok(Headline {
            download_bytes: bytes.map(|(download, _)| download),
            install_bytes: bytes.map(|(_, install)| install),
            version: bundle_config_version(&game_config).unwrap_or_default(),
        });
    }
    let channel = live_channel(&game_config)?;
    let config = channel.get("config").unwrap_or(&Value::Null);

    Ok(Headline {
        download_bytes: config_size_at(config, &["size", "fullSize"]),
        install_bytes: config_size_at(config, &["unCompressSize", "size", "fullSize"]),
        version: channel_version(channel),
    })
}

// ------------ Resource Quality ------------
// Which quality's packs an install uses, switching between them, and the pack records the official launcher reads.
pub(super) fn is_bundle_config(game_config: &Value) -> bool {
    game_config["bundles"].is_object() && game_config["resourcePacks"].is_object()
}

pub(super) fn bundle_name(quality: &str) -> String {
    quality.to_ascii_uppercase()
}

fn bundle_packs(game_config: &Value, bundle: &str) -> Result<Vec<String>, String> {
    let packs: Vec<String> = game_config["bundles"][bundle]["resourcePacks"]
        .as_array()
        .ok_or_else(|| format!("the server offers no {bundle} quality"))?
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    if packs.is_empty() {
        return Err(format!("the {bundle} quality lists no resource packs"));
    }
    Ok(packs)
}

pub(super) fn bundle_config_version(game_config: &Value) -> Option<String> {
    let first_pack = game_config["bundles"]
        .as_object()?
        .values()
        .find_map(|bundle| bundle["resourcePacks"].get(0)?.as_str())?;
    Some(channel_version(&game_config["resourcePacks"][first_pack])).filter(|v| !v.is_empty())
}

pub(super) fn bundle_bytes(game_config: &Value, bundle: &str) -> Option<(u64, u64)> {
    let mut download = 0u64;
    let mut install = 0u64;
    for pack_name in bundle_packs(game_config, bundle).ok()? {
        let pack = &game_config["resourcePacks"][pack_name.as_str()];
        download += config_size_at(pack, &["size"])?;
        install += config_size_at(pack, &["unCompressSize", "size"])?;
    }
    Some((download, install))
}

pub(super) fn selected_quality(app: &AppHandle, profile: &Value) -> Option<String> {
    let default = game_profiles::resource_quality_default(profile)?;
    let key = format!("games.{}.resourceQuality", game_profiles::profile_id(profile));
    let chosen = app.state::<BackendState>().config.get(&key);
    Some(match chosen.as_str() {
        Some(q) if profile["resourceQualityArgs"].get(q).is_some() => q.to_string(),
        _ => default.to_string(),
    })
}

fn recorded_qualities(install_path: &Path) -> Vec<String> {
    std::fs::read_to_string(install_path.join(GAME_CONFIG_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|config| {
            config["bundles"]
                .as_object()
                .map(|bundles| bundles.keys().map(|k| k.to_ascii_lowercase()).collect())
        })
        .unwrap_or_default()
}

fn quality_dir(profile: &Value, install_path: &Path, quality: &str) -> Option<PathBuf> {
    let parts = profile.get("resourceQualityDir")?.as_array()?;
    Some(parts.iter().filter_map(Value::as_str).fold(install_path.to_path_buf(), |dir, part| {
        dir.join(part.replace("{QUALITY}", &bundle_name(quality)))
    }))
}

fn folder_has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

fn folder_is_whole(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut finished = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.contains(".part") {
            return false;
        }
        finished |= name.ends_with(".pak");
    }
    finished
}

fn qualities_in_folders(profile: &Value, install_path: &Path) -> Vec<String> {
    profile["resourceQualityArgs"]
        .as_object()
        .into_iter()
        .flat_map(|qualities| qualities.keys())
        .filter(|q| quality_dir(profile, install_path, q).is_some_and(|dir| folder_is_whole(&dir)))
        .cloned()
        .collect()
}

pub(super) fn qualities_on_disk(profile: &Value, install_path: &Path) -> Vec<String> {
    let Some(default) = game_profiles::resource_quality_default(profile) else {
        return Vec::new();
    };
    let recorded: Vec<String> = recorded_qualities(install_path)
        .into_iter()
        .filter(|q| profile["resourceQualityArgs"].get(q).is_some())
        .collect();
    if profile.get("resourceQualityDir").is_some() {
        let backed: Vec<String> = recorded
            .iter()
            .filter(|q| quality_dir(profile, install_path, q).is_some_and(|dir| folder_has_files(&dir)))
            .cloned()
            .collect();
        if !backed.is_empty() {
            return backed;
        }
        let found = qualities_in_folders(profile, install_path);
        if !found.is_empty() {
            return found;
        }
    }
    if recorded.is_empty() && install_path.join(GAME_CONFIG_FILE).is_file() {
        return vec![default.to_string()];
    }
    recorded
}

pub(super) fn launch_quality(profile: &Value, install_path: &Path, selected: &str) -> String {
    installed_quality(profile, install_path, Some(selected)).unwrap_or_else(|| selected.to_string())
}

pub(super) fn quality_switch_key(profile_id: &str) -> String {
    format!("games.{profile_id}.qualitySwitch")
}

pub(super) fn forget_quality_switch(app: &AppHandle, profile_id: &str) {
    let state = app.state::<BackendState>();
    let key = quality_switch_key(profile_id);
    if state.config.get(&key).is_null() {
        return;
    }
    super::config::set_config_value(app, &key, Value::Null);
    state.config.flush();
}

pub(super) fn installed_quality(
    profile: &Value,
    install_path: &Path,
    preferred: Option<&str>,
) -> Option<String> {
    let on_disk = qualities_on_disk(profile, install_path);
    preferred
        .filter(|p| on_disk.iter().any(|q| q == p))
        .map(str::to_string)
        .or_else(|| on_disk.into_iter().next())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Bundle {
    pub(super) name: String,
    pub(super) packs: Vec<String>,
    pub(super) other_indexes: Vec<String>,
    pub(super) cdns: Vec<String>,
}

fn other_quality_indexes(game_config: &Value, packs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for bundle in game_config["bundles"].as_object().into_iter().flat_map(|b| b.values()) {
        for pack in bundle["resourcePacks"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if packs.iter().any(|p| p == pack) {
                continue;
            }
            let index = game_config["resourcePacks"][pack]["indexFile"].as_str().unwrap_or("").trim();
            if !index.is_empty() && !out.iter().any(|seen| seen == index) {
                out.push(index.to_string());
            }
        }
    }
    out
}

fn remove_unkept_files(install_path: &Path, dests: &[String], keep: &HashSet<String>) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for dest in dests {
        if dest.is_empty() || keep.contains(&super::fs_util::manifest_key(dest)) {
            continue;
        }
        let Ok(path) = super::fs_util::safe_join(install_path, dest) else {
            continue;
        };
        let mut touched = false;
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.is_file() {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        files += 1;
                        bytes += meta.len();
                        touched = true;
                    }
                    Err(e) => log::warn!("Could not remove {dest}: {e}"),
                }
            }
        }
        if let Ok(meta) = std::fs::metadata(peebify_transfer::part::part_path_for(&path)) {
            bytes += meta.len();
            touched = true;
        }
        discard_part(&path);
        if touched {
            super::fs_util::remove_empty_parents(&path, install_path);
        }
    }
    (files, bytes)
}

pub(super) fn note_quality_switch(app: &AppHandle, profile: &Value, install_path: &Path) {
    let Some(quality) = selected_quality(app, profile) else {
        return;
    };
    let on_disk = qualities_on_disk(profile, install_path);
    if on_disk.is_empty() || on_disk.contains(&quality) {
        return;
    }
    let profile_id = game_profiles::profile_id(profile);
    log::info!(
        "{profile_id} is switching to {} quality ({} on disk).",
        bundle_name(&quality),
        on_disk.iter().map(|q| bundle_name(q)).collect::<Vec<_>>().join(" + ")
    );
    super::config::set_config_value(app, &quality_switch_key(profile_id), json!(quality));
    app.state::<BackendState>().config.flush();
}

async fn remove_other_qualities(profile: &Value, install_path: &Path, bundle: &Bundle, kept: &[Entry]) {
    if bundle.other_indexes.is_empty() {
        return;
    }
    let name = game_profiles::display_name(profile);
    let mut dests: Vec<String> = Vec::new();
    for index_file in &bundle.other_indexes {
        match fetch_index(name, &bundle.cdns, index_file).await {
            Ok((_, index)) => dests.extend(index_entries(&index, "").into_iter().map(|e| e.path.into_string())),
            Err(e) => log::warn!("{name}: could not read {index_file}, so its files stay: {e}"),
        }
    }
    let keep: HashSet<String> = kept
        .iter()
        .map(|e| super::fs_util::manifest_key(&e.path))
        .collect();
    let dir = install_path.to_path_buf();
    let (files, bytes) =
        tauri::async_runtime::spawn_blocking(move || remove_unkept_files(&dir, &dests, &keep))
            .await
            .unwrap_or((0, 0));
    if bytes > 0 {
        log::info!(
            "{name}: removed {files} file(s) the {} quality doesn't use, {:.2} GB freed.",
            bundle.name,
            progress::gib(bytes as f64)
        );
    }
}


pub(super) fn write_pack_records(install_path: &Path, packs: &[String], version: &str) -> Result<(), String> {
    let dir = install_path.join(PACK_RECORD_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for pack in packs {
        let record = json!({ "packName": pack, "version": version });
        super::fs_util::write_atomic(&dir.join(format!("{pack}.json")), record.to_string().as_bytes())?;
    }
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        let stale = path.extension().is_some_and(|ext| ext == "json")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| !packs.iter().any(|p| p == stem));
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

pub(super) fn app_id(profile: &Value) -> Option<&str> {
    let url = profile.get("gameConfigUrl")?.as_str()?;
    url.split('/').find_map(|segment| {
        let (id, key) = segment.split_once('_')?;
        (!id.is_empty() && !key.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
            .then_some(id)
    })
}

// ------------ Install Preview ------------
// The download and install size of the selected quality, and which qualities are on disk.
pub(super) async fn preview(profile: &Value, quality: Option<&str>) -> Result<Preview, String> {
    let headline = headline(profile, quality).await?;
    if let (Some(download), false) = (headline.download_bytes, headline.version.is_empty()) {
        return Ok(Preview {
            version: headline.version,
            download_bytes: download,
            install_bytes: headline.install_bytes.unwrap_or(download),
            file_count: 0,
            approximate: false,
            parts: Vec::new(),
        });
    }
    Ok(resource_list_preview(resolve(profile, quality).await?.0))
}

pub(super) async fn get_resource_quality(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let game_id = match args.first().and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => app.state::<BackendState>().config.active_game_id(),
    };
    let profile = game_profiles::profile(&game_id);
    let Some(qualities) = profile["resourceQualityArgs"].as_object() else {
        return Ok(ok_with(json!({ "installed": [], "sizes": null })));
    };
    let key = format!("games.{}.gamePath", game_profiles::profile_id(profile));
    let game_path = app.state::<BackendState>().config.get(&key);
    let installed = match game_path.as_str().filter(|p| !p.is_empty()) {
        Some(path) => qualities_on_disk(profile, std::path::Path::new(path)),
        None => Vec::new(),
    };

    let with_sizes = args.get(1).and_then(Value::as_bool).unwrap_or(false);
    if !with_sizes {
        return Ok(ok_with(json!({ "installed": installed, "sizes": Value::Null })));
    }

    let config_url = profile["gameConfigUrl"].as_str().unwrap_or_default();
    let sizes = match http::get_json(config_url).await {
        Ok(config) if is_bundle_config(&config) => {
            let mut sizes = serde_json::Map::new();
            for quality in qualities.keys() {
                let bundle = bundle_name(quality);
                if let Some((download, install)) = bundle_bytes(&config, &bundle) {
                    sizes.insert(
                        quality.clone(),
                        json!({ "downloadBytes": download, "installBytes": install }),
                    );
                }
            }
            Value::Object(sizes)
        }
        Ok(_) => Value::Null,
        Err(e) => {
            log::debug!("Resource quality sizes unavailable for {game_id}: {e}");
            Value::Null
        }
    };
    Ok(ok_with(json!({ "installed": installed, "sizes": sizes })))
}

// ------------ Quality Check ------------
// Compares what is selected with what is on disk and with what a launch would pass the game, counting each quality's
// own files against its file list. It changes nothing: the answer decides whether Apply or a repair is what is needed.

fn exclusive_packs(game_config: &Value, bundle: &str) -> Result<Vec<String>, String> {
    let packs = bundle_packs(game_config, bundle)?;
    let others: HashSet<&str> = game_config["bundles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.as_str() != bundle)
        .flat_map(|(_, other)| other["resourcePacks"].as_array().into_iter().flatten().filter_map(Value::as_str))
        .collect();
    Ok(packs.into_iter().filter(|pack| !others.contains(pack.as_str())).collect())
}

fn count_present(install_path: &Path, entries: &[Entry]) -> (usize, usize) {
    let present = entries
        .iter()
        .filter(|entry| {
            super::fs_util::safe_join(install_path, &entry.path)
                .ok()
                .and_then(|path| std::fs::metadata(path).ok())
                .is_some_and(|meta| meta.is_file() && meta.len() == entry.size)
        })
        .count();
    (present, entries.len())
}

pub(super) async fn check_resource_quality(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let game_id = match args.first().and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => app.state::<BackendState>().config.active_game_id(),
    };
    let profile = game_profiles::profile(&game_id);
    let profile_id = game_profiles::profile_id(profile);
    let name = game_profiles::display_name(profile);
    let (Some(qualities), Some(selected)) =
        (profile["resourceQualityArgs"].as_object(), selected_quality(app, profile))
    else {
        return Ok(err_response(format!("{name} has no resource quality to check.")));
    };
    let game_path = app.state::<BackendState>().config.get(&format!("games.{profile_id}.gamePath"));
    let Some(path) = game_path.as_str().filter(|p| !p.is_empty()).map(PathBuf::from).filter(|p| p.is_dir()) else {
        return Ok(err_response(format!("{name} isn't installed.")));
    };
    let config_url = profile["gameConfigUrl"].as_str().unwrap_or_default();
    let game_config = match http::with_retry(|| http::get_json(config_url), 3, 1000, "Kuro game config").await {
        Ok(config) if is_bundle_config(&config) => config,
        Ok(_) => return Ok(err_response(format!("{name}'s server offers no resource qualities."))),
        Err(e) => return Ok(err_response(e)),
    };
    let cdns = cdn_urls(&game_config);
    let on_disk = qualities_on_disk(profile, &path);

    let mut files = serde_json::Map::new();
    for quality in qualities.keys() {
        let there = on_disk.contains(quality)
            || *quality == selected
            || quality_dir(profile, &path, quality).is_some_and(|dir| folder_has_files(&dir));
        if !there {
            continue;
        }
        let Ok(packs) = exclusive_packs(&game_config, &bundle_name(quality)) else {
            continue;
        };
        let mut entries = Vec::new();
        for pack in &packs {
            let index_file = game_config["resourcePacks"][pack.as_str()]["indexFile"].as_str().unwrap_or("").trim();
            if index_file.is_empty() {
                continue;
            }
            match fetch_index(name, &cdns, index_file).await {
                Ok((_, index)) => entries.extend(index_entries(&index, "")),
                Err(e) => return Ok(err_response(e)),
            }
        }
        let dir = path.clone();
        let (present, total) = tauri::async_runtime::spawn_blocking(move || count_present(&dir, &entries))
            .await
            .unwrap_or((0, 0));
        files.insert(quality.clone(), json!({ "present": present, "total": total }));
    }

    let launch_with = launch_quality(profile, &path, &selected);
    log::info!(
        "{name}: quality check: selected {selected}, on disk {on_disk:?}, launches at {launch_with}, files {}",
        Value::Object(files.clone())
    );
    Ok(ok_with(json!({ "selected": selected, "launchWith": launch_with, "onDisk": on_disk, "files": files })))
}

// ------------ Tests ------------
// Config parsing, quality bundles and the records a bundle install leaves for the official launcher.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;
    use crate::backend::validator::{update_game_config_file, write_game_config};
    use peebify_transfer::Sealed;

    #[test]
    fn kuro_indexes_are_read_under_either_key() {
        let entry = json!({ "dest": "a.pak", "size": 4, "md5": "x" });

        let either = index_entries(&json!({ "resources": [entry] }), "https://cdn.example/zip/");
        assert_eq!(index_entries(&json!({ "resource": [entry] }), "").len(), 1);
        assert_eq!(either.len(), 1);
        assert_eq!(either[0].url(), "https://cdn.example/zip/a.pak");
        assert!(either[0].live, "a Kuro file replaces one the game runs from");
        assert!(index_entries(&json!({ "resource": [] }), "").is_empty());
        assert!(index_entries(&json!({}), "").is_empty());
    }

    #[test]
    fn a_quality_switch_finds_the_packs_only_other_qualities_use() {
        let config = json!({
            "bundles": {
                "SD": { "resourcePacks": ["common", "sd"] },
                "HD": { "resourcePacks": ["common", "hd"] },
                "UHD": { "resourcePacks": ["common", "uhd"] }
            },
            "resourcePacks": {
                "common": { "indexFile": "common.json" },
                "sd": { "indexFile": "sd.json" },
                "hd": { "indexFile": "hd.json" },
                "uhd": { "indexFile": " " }
            }
        });
        let uhd = vec!["common".to_string(), "uhd".to_string()];
        assert_eq!(other_quality_indexes(&config, &uhd), vec!["sd.json", "hd.json"]);
    }

    #[test]
    fn removing_another_quality_keeps_shared_files_and_clears_its_unfinished_parts() {
        let dir = TempDir::new("other-qualities");
        let root = dir.path();
        std::fs::create_dir_all(root.join("Client/Content/SD")).unwrap();
        std::fs::create_dir_all(root.join("Client/Content/HD")).unwrap();
        std::fs::create_dir_all(root.join("Client/Content/Paks")).unwrap();
        std::fs::write(root.join("Client/Content/SD/a-SD.pak"), b"sdsd").unwrap();
        std::fs::write(root.join("Client/Content/HD/b-HD.pak.part"), b"hd").unwrap();
        std::fs::write(root.join("Client/Content/HD/b-HD.pak.part.segments.json"), b"{}").unwrap();
        std::fs::write(root.join("Client/Content/Paks/shared.pak"), b"shared").unwrap();
        let dests = vec![
            "Client/Content/SD/a-SD.pak".to_string(),
            "Client/Content/HD/b-HD.pak".to_string(),
            "Client/Content/Paks/shared.pak".to_string(),
            "Client/Content/SD/missing.pak".to_string(),
        ];
        let keep: HashSet<String> = [crate::backend::fs_util::manifest_key("Client/Content/Paks/shared.pak")].into();

        let (files, bytes) = remove_unkept_files(root, &dests, &keep);

        assert_eq!((files, bytes), (1, 6));
        assert!(!root.join("Client/Content/SD").exists(), "the emptied quality folder goes");
        assert!(!root.join("Client/Content/HD").exists(), "an unfinished part and its record go");
        assert!(root.join("Client/Content/Paks/shared.pak").exists());
    }

    #[test]
    fn each_kuro_game_records_its_own_app_id() {
        let wuwa = json!({ "gameConfigUrl": "https://cdn.example.com/launcher/game/G153/50004_obOH/index.json" });
        let pgr = json!({ "gameConfigUrl": "https://cdn.example.com/launcher/game/G143/50015_LWdk/index.json" });
        assert_eq!(app_id(&wuwa), Some("50004"));
        assert_eq!(app_id(&pgr), Some("50015"));
        assert_eq!(app_id(&json!({ "installMode": "gf2" })), None);
        assert_eq!(app_id(&json!({ "gameConfigUrl": "https://x.example.com/a/index.json" })), None);
        let bundled = json!({ "gameConfigUrl": "https://cdn.example.com/launcher/game/50004_P7xc/G153/official/index.json" });
        assert_eq!(app_id(&bundled), Some("50004"));

        let dir = TempDir::new("game-config");
        write_game_config(&Sealed::nothing_written(), dir.path(), " 2.4.0 ", app_id(&pgr), None).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(GAME_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(written["version"], json!("2.4.0"));
        assert_eq!(written["appId"], json!("50015"));
        update_game_config_file(&Sealed::nothing_written(), dir.path(), "3.0").unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(GAME_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(written["version"], json!("3.0"));
        assert!(written.get("appId").is_none());
        assert!(!dir.path().join(format!("{GAME_CONFIG_FILE}.tmp")).exists());
    }

    fn bundle_config() -> Value {
        let pack = |name: &str, size: u64| {
            json!({
                "version": "3.7.0",
                "indexFile": format!("launcher/game/G153/50004/3.7.0/abc/{name}/indexFile.json"),
                "baseUrl": "launcher/game/G153/50004/3.7.0/abc/zip/",
                "size": size,
                "unCompressSize": size,
            })
        };
        json!({
            "cdnList": [{ "url": "https://cdn.example.com/" }],
            "resourcePacks": {
                "common": pack("common", 40), "sd": pack("sd", 22), "hd": pack("hd", 46), "uhd": pack("uhd", 66)
            },
            "bundles": {
                "SD": { "resourcePacks": ["common", "sd"] },
                "HD": { "resourcePacks": ["common", "hd"] },
                "UHD": { "resourcePacks": ["common", "uhd"] }
            }
        })
    }

    #[test]
    fn a_bundle_config_resolves_each_quality_to_its_packs() {
        let config = bundle_config();
        assert!(is_bundle_config(&config));
        assert!(!is_bundle_config(&json!({ "default": { "version": "3.7.0" } })));
        assert_eq!(bundle_packs(&config, &bundle_name("uhd")).unwrap(), ["common", "uhd"]);
        assert!(bundle_packs(&config, "8K").is_err());
        assert_eq!(bundle_config_version(&config).as_deref(), Some("3.7.0"));
        assert_eq!(bundle_bytes(&config, "SD"), Some((62, 62)));
        assert_eq!(bundle_bytes(&config, "UHD"), Some((106, 106)));
    }

    #[test]
    fn a_bundle_install_is_recorded_the_way_the_official_launcher_reads_it() {
        let dir = TempDir::new("bundle-config");
        let records = dir.path().join(PACK_RECORD_DIR);
        std::fs::create_dir_all(&records).unwrap();
        std::fs::write(records.join("hd.json"), "{}").unwrap();

        let sd = Bundle {
            name: "SD".into(),
            packs: vec!["common".into(), "sd".into()],
            ..Bundle::default()
        };
        write_game_config(&Sealed::nothing_written(), dir.path(), "3.7.0", Some("50004"), Some(&sd)).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(GAME_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(written["appId"], json!("50004"));
        assert_eq!(written["bundles"]["SD"]["resourcePacks"], json!(["common", "sd"]));
        assert!(written.get("reUseVersion").is_none());
        assert!(records.join("common.json").is_file());
        assert!(records.join("sd.json").is_file());
        assert!(!records.join("hd.json").exists());

        update_game_config_file(&Sealed::nothing_written(), dir.path(), "3.8.0").unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(GAME_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(written["version"], json!("3.8.0"));
        assert_eq!(written["bundles"]["SD"]["version"], json!("3.8.0"));
    }

    #[test]
    fn the_installed_quality_comes_from_the_recorded_bundles() {
        let wuwa = game_profiles::profile("wuwa");
        let dir = TempDir::new("installed-quality");
        assert!(qualities_on_disk(wuwa, dir.path()).is_empty());
        assert_eq!(installed_quality(wuwa, dir.path(), Some("sd")), None);

        std::fs::write(dir.path().join(GAME_CONFIG_FILE), r#"{"version":"3.6.1"}"#).unwrap();
        assert_eq!(qualities_on_disk(wuwa, dir.path()), ["hd"]);

        std::fs::write(
            dir.path().join(GAME_CONFIG_FILE),
            r#"{"version":"3.7.0","bundles":{"HD":{},"UHD":{}}}"#,
        )
        .unwrap();
        assert_eq!(installed_quality(wuwa, dir.path(), Some("uhd")).as_deref(), Some("uhd"));
        assert_eq!(installed_quality(wuwa, dir.path(), Some("sd")).as_deref(), Some("hd"));
        assert!(qualities_on_disk(game_profiles::profile("pgr"), dir.path()).is_empty());
    }

    fn quality_folder(root: &Path, quality: &str, files: &[&str]) {
        let dir = root.join("Client/Content").join(quality);
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            std::fs::write(dir.join(file), b"x").unwrap();
        }
    }

    #[test]
    fn a_copy_nothing_recorded_is_read_from_its_quality_folders() {
        let wuwa = game_profiles::profile("wuwa");
        let dir = TempDir::new("steam-copy");
        quality_folder(dir.path(), "UHD", &["pakchunk1-UHD-WindowsNoEditor.pak", "pakchunk1-UHD-WindowsNoEditor.sig"]);
        quality_folder(dir.path(), "HD", &["pakchunk1-HD-WindowsNoEditor.pak", "pakchunk2-HD-WindowsNoEditor.pak.part"]);
        quality_folder(dir.path(), "SD", &[]);
        assert_eq!(qualities_on_disk(wuwa, dir.path()), ["uhd"], "an unfinished download or an empty folder is not a quality");
        assert_eq!(launch_quality(wuwa, dir.path(), "hd"), "uhd", "the quality that is there beats the one that is selected");
        assert_eq!(launch_quality(wuwa, dir.path(), "uhd"), "uhd");
    }

    #[test]
    fn a_record_for_a_quality_that_is_not_on_disk_is_not_believed() {
        let wuwa = game_profiles::profile("wuwa");
        let dir = TempDir::new("wrong-record");
        std::fs::write(dir.path().join(GAME_CONFIG_FILE), r#"{"version":"3.7.1","bundles":{"UHD":{}}}"#).unwrap();
        quality_folder(dir.path(), "HD", &["pakchunk1-HD-WindowsNoEditor.pak"]);
        assert_eq!(qualities_on_disk(wuwa, dir.path()), ["hd"]);
        assert_eq!(launch_quality(wuwa, dir.path(), "uhd"), "hd", "the game is launched at what it has");

        let empty = TempDir::new("record-only");
        std::fs::write(empty.path().join(GAME_CONFIG_FILE), r#"{"version":"3.7.1","bundles":{"UHD":{}}}"#).unwrap();
        assert_eq!(qualities_on_disk(wuwa, empty.path()), ["uhd"], "folders that say nothing leave the record standing");
    }

    #[test]
    fn a_recorded_quality_wins_over_a_half_finished_switch() {
        let wuwa = game_profiles::profile("wuwa");
        let dir = TempDir::new("half-switch");
        std::fs::write(dir.path().join(GAME_CONFIG_FILE), r#"{"version":"3.7.1","bundles":{"HD":{}}}"#).unwrap();
        quality_folder(dir.path(), "HD", &["pakchunk1-HD-WindowsNoEditor.pak"]);
        quality_folder(dir.path(), "UHD", &["pakchunk1-UHD-WindowsNoEditor.pak"]);
        assert_eq!(qualities_on_disk(wuwa, dir.path()), ["hd"], "UHD is not recorded, so its files are not yet the install");
    }

    #[test]
    fn a_quality_is_checked_against_the_packs_only_it_uses() {
        let config = bundle_config();
        assert_eq!(exclusive_packs(&config, "UHD").unwrap(), ["uhd"]);
        assert_eq!(exclusive_packs(&config, "SD").unwrap(), ["sd"]);
        assert!(exclusive_packs(&config, "8K").is_err());

        let dir = TempDir::new("count-present");
        std::fs::create_dir_all(dir.path().join("Client/Content/UHD")).unwrap();
        std::fs::write(dir.path().join("Client/Content/UHD/a.pak"), b"1234").unwrap();
        std::fs::write(dir.path().join("Client/Content/UHD/b.pak"), b"12").unwrap();
        let entries = index_entries(
            &json!({ "resource": [
                { "dest": "Client/Content/UHD/a.pak", "size": 4, "md5": "x" },
                { "dest": "Client/Content/UHD/b.pak", "size": 9, "md5": "x" },
                { "dest": "Client/Content/UHD/c.pak", "size": 1, "md5": "x" }
            ]}),
            "",
        );
        assert_eq!(count_present(dir.path(), &entries), (1, 3), "a file of the wrong size or a missing one is not in place");
    }

    #[test]
    fn an_install_with_no_version_file_is_checked_by_checksum_and_recorded() {
        let dir = TempDir::new("kuro-policy");
        let policy = Kuro::default().policy(dir.path());
        assert!(policy.checksums && policy.records, "a Steam copy learns its version from its first repair");
        std::fs::write(dir.path().join(GAME_CONFIG_FILE), r#"{"version":"3.7.0"}"#).unwrap();
        let policy = Kuro::default().policy(dir.path());
        assert!(!policy.checksums && !policy.records && !policy.latest);
    }

    #[test]
    fn a_null_channel_version_is_empty_not_the_word_null() {
        assert_eq!(channel_version(&json!({ "version": null })), "");
        assert_eq!(channel_version(&json!({})), "");
        assert_eq!(channel_version(&json!({ "version": "2.5.1" })), "2.5.1");
        assert_eq!(channel_version(&json!({ "version": 3 })), "3");
        assert!(live_channel(&json!({ "default": null })).is_err());
        assert!(live_channel(&json!({ "predownload": {} })).is_err());
        assert!(live_channel(&json!({ "default": {} })).is_ok());
    }

    #[test]
    fn every_kuro_cdn_is_kept_in_order_without_repeats() {
        let channel = json!({ "cdnList": [
            { "url": "https://a.example.com/" },
            { "url": "" },
            { "url": "https://b.example.com/" },
            { "url": "https://a.example.com/" },
            { "K": 1 },
        ]});
        assert_eq!(
            cdn_urls(&channel),
            vec!["https://a.example.com/".to_string(), "https://b.example.com/".to_string()]
        );
        assert!(cdn_urls(&json!({})).is_empty());
    }
}
