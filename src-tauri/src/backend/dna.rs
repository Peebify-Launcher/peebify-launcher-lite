// ------------ Duet Night Abyss ------------
// Talks to Pan Studio's CDN for Duet Night Abyss. A build ships as one HDiffPatch directory archive (.hdiff): a full archive unpacks the game
// from nothing, a diff archive patches an older build in place. Both are applied by the bundled hpatchz. The CDN also lists every file's MD5
// per build (Hash.json), which is what verification checks, and the official launcher's banner and notice feeds, which drive the news panel.
// It holds the game's publisher (what the install, repair and verify reconcile to), its install preview and its news.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use peebify_transfer::part::with_suffix;
use peebify_transfer::Sealed;
use serde_json::{json, Value};

use super::engine::{package_preview, JobCore, JobKind, Preview};
use super::extract::{run_hpatchz, Extracted, VERIFIED_SUFFIX};
use super::fs_util;
use super::game_manager::local_game_version_at;
use super::game_profiles::{self, profile_str};
use super::manifest::{json_u64, Container, Cx, Entry, Hash, Manifest, Policy, Publisher, Unpacked, Unpacker};
use super::news::{article, as_i64, as_str, envelope, local_month_day, slide, MAX_ITEMS, MAX_SLIDES};
use super::validator::{self, Triage};
use super::http;

pub const VERSION_FILE: &str = "GameVersion.json";
pub const PATCH_DIR: &str = "PeebifyPatch";
pub const HPATCHZ: &str = "hpatchz.exe";

pub fn cdns(profile: &Value) -> Vec<String> {
    profile
        .get("dnaCdns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|cdn| cdn.trim().trim_end_matches('/').to_string())
        .filter(|cdn| cdn.starts_with("https://"))
        .collect()
}

fn package_root(profile: &Value) -> Result<&str, String> {
    match profile_str(profile, "dnaPackagePath").trim_matches('/') {
        "" => Err(format!(
            "{} is missing its dnaPackagePath setting.",
            game_profiles::display_name(profile)
        )),
        root => Ok(root),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub latest: u64,
    pub number: String,
    pub min_supported: u64,
}

pub fn build_from(body: &Value) -> Result<Build, String> {
    let latest = json_u64(body.get("latest_version"))
        .filter(|v| *v > 0)
        .ok_or("the version manifest has no latest_version")?;
    let number = as_str(body.get("latest_version_number")).trim().to_string();
    let well_formed = !number.is_empty()
        && number.split('.').count() <= 2
        && number.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    if !well_formed {
        return Err(format!("the version manifest has a malformed latest_version_number ({number:?})"));
    }
    if !latest.to_string().starts_with(&number.replace('.', "")) {
        return Err(format!(
            "the version manifest's latest_version {latest} does not belong to version {number}"
        ));
    }
    let min_supported = json_u64(body.get("min_supported_version")).unwrap_or(0);
    Ok(Build { latest, number, min_supported })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub version: u64,
    pub file_name: String,
    pub md5: String,
    pub download_bytes: u64,
    pub install_bytes: u64,
    pub urls: Vec<String>,
    pub full: bool,
}

pub fn package_dir(build: &Build, from: Option<u64>) -> String {
    match from {
        Some(from) => format!("{}/{}/{from}_{}", build.number, build.latest, build.latest),
        None => format!("{}/{}/full_{}", build.number, build.latest, build.latest),
    }
}

pub fn package_from(body: &Value, version: u64, full: bool, folder_urls: &[String]) -> Result<Package, String> {
    let info = body.get("hdiff_file").ok_or("the archive record has no hdiff_file")?;
    let file_name = as_str(info.get("name")).trim().to_string();
    let plain = !file_name.is_empty()
        && file_name.ends_with(".hdiff")
        && file_name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !file_name.contains("..");
    if !plain {
        return Err(format!("the archive record names an unexpected file ({file_name:?})"));
    }
    let md5 = as_str(info.get("md5")).trim().to_ascii_lowercase();
    if md5.len() != 32 || !md5.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("the archive record has no usable MD5".to_string());
    }
    let download_bytes = json_u64(info.get("size")).filter(|s| *s > 0).ok_or("the archive record has no size")?;
    let install_bytes = json_u64(body.get("new_size")).unwrap_or(0);
    Ok(Package {
        version,
        urls: folder_urls.iter().map(|folder| format!("{folder}/{file_name}")).collect(),
        file_name,
        md5,
        download_bytes,
        install_bytes,
        full,
    })
}

async fn get_json_from_any(profile: &Value, rel: &str) -> Result<Value, String> {
    let mirrors = cdns(profile);
    if mirrors.is_empty() {
        return Err(format!("{} has no download mirrors configured.", game_profiles::display_name(profile)));
    }
    let mut last = String::new();
    for cdn in &mirrors {
        match http::get_json(&format!("{cdn}/{rel}")).await {
            Ok(body) => return Ok(body),
            Err(e) => last = e,
        }
    }
    Err(last)
}

pub async fn fetch_build(profile: &Value) -> Result<Build, String> {
    let root = package_root(profile)?;
    build_from(&get_json_from_any(profile, &format!("{root}/VersionManifest.json")).await?)
}

async fn fetch_package_at(profile: &Value, build: &Build, from: Option<u64>) -> Result<Package, String> {
    let root = package_root(profile)?;
    let dir = package_dir(build, from);
    let body = get_json_from_any(profile, &format!("{root}/{dir}/HPatchDiffMd5.json")).await?;
    let folders: Vec<String> = cdns(profile).iter().map(|cdn| format!("{cdn}/{root}/{dir}")).collect();
    package_from(&body, build.latest, from.is_none(), &folders)
}

pub async fn fetch_package(profile: &Value, build: &Build, installed: Option<u64>, repair: bool) -> Result<Package, String> {
    let patchable = installed.filter(|v| !repair && *v >= build.min_supported && *v < build.latest);
    if let Some(from) = patchable {
        match fetch_package_at(profile, build, Some(from)).await {
            Ok(package) => return Ok(package),
            Err(e) => log::warn!(
                "{}: no diff archive from {from} to {} ({e}); downloading the full game instead.",
                game_profiles::display_name(profile),
                build.latest
            ),
        }
    }
    fetch_package_at(profile, build, None).await
}

pub fn installed_version(game_path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(game_path.join(VERSION_FILE)).ok()?;
    let body: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    json_u64(body.get("version")).filter(|v| *v > 0)
}

pub fn write_installed_version(game_path: &Path, version: u64) -> Result<(), String> {
    let text = format!("{{\n    \"version\": {version}\n}}");
    fs_util::write_atomic(&game_path.join(VERSION_FILE), text.as_bytes())
        .map_err(|e| format!("Could not record the installed Duet Night Abyss version: {e}"))
}

pub fn files_from(body: &Value, game_path: &Path) -> Vec<Entry> {
    let mut files: Vec<Entry> = body
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(path, entry)| {
            let path = path.trim().trim_start_matches(['/', '\\']).replace('\\', "/");
            let safe = !path.is_empty() && !path.split('/').any(|part| part.is_empty() || part == "." || part == "..");
            let md5 = as_str(entry.get("md5")).trim();
            let size = fs_util::safe_join(game_path, &path).ok().and_then(|p| std::fs::metadata(p).ok()).filter(|m| m.is_file());
            (safe && md5.len() == 32 && md5.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| Entry::new(path, size.map_or(0, |m| m.len()), Hash::parse(md5), ""))
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

async fn fetch_files(profile: &Value, build: &Build, game_path: &Path) -> Result<Vec<Entry>, String> {
    let root = package_root(profile)?;
    let rel = format!("{root}/{}/{}/Hash.json", build.number, build.latest);
    let body = get_json_from_any(profile, &rel).await?;
    let game_path = game_path.to_path_buf();
    let files = tauri::async_runtime::spawn_blocking(move || files_from(&body, &game_path))
        .await
        .map_err(|e| format!("file list task panicked: {e}"))?;
    if files.is_empty() {
        return Err("the file list for this build is empty".to_string());
    }
    Ok(files)
}

// ------------ Install Preview ------------
// The size of the full archive.
pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    let package = fetch_package(profile, &fetch_build(profile).await?, None, false).await?;
    Ok(package_preview(package.version.to_string(), package.download_bytes, package.install_bytes, 1))
}

// ------------ Publisher ------------
// The build's archive is the only way to its files: it is fetched into a patch folder beside the game and applied by
// hpatchz, a full archive over the folder and a diff into a copy of it that is swapped in. A repair and a verify check
// the files Hash.json lists, and a repair applies the full archive again. An archive is noted once it is known to be
// whole, so a retry after a failed patch does not hash it again.
pub(super) struct Dna;

impl Publisher for Dna {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let name = game_profiles::display_name(cx.profile);
            let build = fetch_build(cx.profile).await?;
            let installed = installed_version(cx.dir);
            let mut manifest = Manifest { version: build.latest.to_string(), adopts: installed.is_none(), ..Default::default() };
            if cx.purpose != JobKind::Install && installed.is_some_and(|v| v != build.latest) {
                return Ok(manifest);
            }
            if cx.purpose != JobKind::Install {
                manifest.entries = fetch_files(cx.profile, &build, cx.dir)
                    .await
                    .map_err(|e| format!("Could not load the {name} file list: {e}"))?;
            }
            if cx.purpose == JobKind::Verify {
                return Ok(manifest);
            }
            let package = fetch_package(cx.profile, &build, installed, cx.purpose == JobKind::Repair).await?;
            log::info!(
                "{name} {} archive for build {} — {:.2}GB to download, {:.2}GB installed.",
                if package.full { "full" } else { "diff" },
                package.version,
                peebify_transfer::progress::gib(package.download_bytes as f64),
                peebify_transfer::progress::gib(package.install_bytes as f64),
            );
            let hpatchz = fs_util::resource(cx.app, HPATCHZ).ok_or_else(|| {
                format!("{name} is applied with {HPATCHZ}, which is missing from Peebify's install. Reinstall Peebify to restore it.")
            })?;
            let room = if cx.purpose != JobKind::Install {
                0
            } else if package.full {
                let dir = cx.dir.to_path_buf();
                let held = tauri::async_runtime::spawn_blocking(move || {
                    fs_util::directory_size(&dir).saturating_sub(fs_util::directory_size(&dir.join(PATCH_DIR)))
                })
                .await
                .unwrap_or(0);
                package.install_bytes.saturating_sub(held)
            } else {
                package.install_bytes
            };
            let folders: Vec<String> = package
                .urls
                .iter()
                .filter_map(|url| url.strip_suffix(&format!("/{}", package.file_name)))
                .map(str::to_string)
                .collect();
            let url = package.urls.first().ok_or("no download mirror for the archive")?;
            let part = Entry::new(format!("{PATCH_DIR}/{}", package.file_name), package.download_bytes, Hash::parse(&package.md5), url.as_str());
            let unpack = Hpatch { hpatchz, md5: package.md5, bytes: package.install_bytes, full: package.full };
            manifest.containers = vec![Container { parts: vec![part], unpack: Some(Arc::new(unpack)), room }];
            manifest.mirrors = folders;
            Ok(manifest)
        })
    }

    fn triage(&self, dir: &Path, files: &[Entry]) -> Triage {
        let noted = |part: &Entry| {
            fs_util::safe_join(dir, &part.path).is_ok_and(|path| {
                std::fs::metadata(&path).is_ok_and(|meta| meta.len() == part.size)
                    && std::fs::read_to_string(with_suffix(&path, VERIFIED_SUFFIX)).is_ok_and(|note| note.trim() == part.hash.expected())
            })
        };
        Triage { trusted: files.iter().filter(|f| noted(f)).map(|f| fs_util::manifest_key(&f.path)).collect(), ..Triage::default() }
    }

    fn after_seal<'a>(
        &'a self,
        _job: &'a Arc<JobCore>,
        dir: &'a Path,
        manifest: &'a Manifest,
        sealed: &'a Sealed,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            write_installed_version(dir, manifest.version.parse().map_err(|_| format!("{:?} is not a build number.", manifest.version))?)?;
            validator::update_game_config_file(sealed, dir, &manifest.version)?;
            let patch_dir = dir.join(PATCH_DIR);
            match std::fs::remove_dir_all(&patch_dir) {
                Ok(()) => log::info!("Removed {} now that the build is installed.", patch_dir.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("Could not remove {} after installing ({e}); it is safe to delete by hand.", patch_dir.display()),
            }
            Ok(true)
        })
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({ "version": fetch_build(profile).await?.latest.to_string() })) })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn write_space(&self, download: u64, install: u64) -> (u64, f64) {
        (download.saturating_add(install), 1.0)
    }

    fn installed_version(&self, dir: &Path) -> Option<String> {
        local_game_version_at(dir).or_else(|| installed_version(dir).map(|v| v.to_string()))
    }

    fn policy(&self, _dir: &Path) -> Policy {
        Policy { checksums: true, records: true, ..Policy::default() }
    }
}

struct Hpatch {
    hpatchz: PathBuf,
    md5: String,
    bytes: u64,
    full: bool,
}

impl Unpacker for Hpatch {
    fn is_package(&self, name: &str) -> bool {
        name.ends_with(".hdiff")
    }

    fn unpack<'a>(
        &'a self,
        job: &'a Arc<JobCore>,
        dir: &'a Path,
        archive: &'a Path,
        _wanted: Option<&'a HashSet<String>>,
    ) -> BoxFuture<'a, Result<Unpacked, String>> {
        Box::pin(async move {
            let name = game_profiles::display_name(job.profile());
            let patch_dir = dir.join(PATCH_DIR);
            if let Err(e) = fs_util::write_atomic(&with_suffix(archive, VERIFIED_SUFFIX), self.md5.as_bytes()) {
                log::warn!("{name}: could not remember the checked archive ({e}).");
            }
            let report = job.unpacking(archive, self.bytes, 1, format!("Installing {name}..."));
            clear_patch_leftovers(dir);
            let full = self.full;
            let baseline = match full {
                true => {
                    let (dir, patch_dir) = (dir.to_path_buf(), patch_dir.clone());
                    tauri::async_runtime::spawn_blocking(move || fs_util::directory_size(&dir).saturating_sub(fs_util::directory_size(&patch_dir)))
                        .await
                        .unwrap_or(0)
                }
                false => 0,
            };
            let watching = Arc::new(AtomicBool::new(true));
            let watcher = {
                let (watching, report, total) = (Arc::clone(&watching), Arc::clone(&report), self.bytes as f64);
                let (install, patch_dir) = (dir.to_path_buf(), patch_dir.clone());
                tauri::async_runtime::spawn(async move {
                    while watching.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let (install, patch_dir) = (install.clone(), patch_dir.clone());
                        let written = tauri::async_runtime::spawn_blocking(move || {
                            bytes_written(&install, &patch_dir, full).saturating_sub(baseline)
                        })
                        .await
                        .unwrap_or(0);
                        if total > 0.0 && watching.load(Ordering::Relaxed) {
                            report((written as f64 / total * 100.0).min(99.0));
                        }
                    }
                })
            };

            let old_dir = (!full).then_some(dir);
            let patched = job
                .gate
                .cancellable(run_hpatchz(&self.hpatchz, old_dir, archive, dir))
                .await
                .map_err(|e| job.cancel_aware(e));
            watching.store(false, Ordering::Relaxed);
            watcher.abort();
            if patched.is_err() {
                clear_patch_leftovers(dir);
            }
            patched?;
            report(100.0);
            Ok(Unpacked { written: Extracted::Tree { root: dir.to_path_buf(), skip: vec![patch_dir] }, files: Vec::new() })
        })
    }
}

fn patch_temp_dirs(install_path: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(folder)) = (install_path.parent(), install_path.file_name()) else {
        return Vec::new();
    };
    let folder = folder.to_string_lossy().into_owned();
    let Ok(entries) = std::fs::read_dir(parent) else { return Vec::new() };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix(&folder)
                .and_then(|rest| rest.strip_suffix(".tmp"))
                .is_some_and(|digits| digits.len() == 3 && digits.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|entry| entry.path())
        .collect()
}

fn clear_patch_leftovers(install_path: &Path) {
    for dir in patch_temp_dirs(install_path) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => log::info!("Removed {}, left by an interrupted Duet Night Abyss patch.", dir.display()),
            Err(e) => log::warn!("Could not remove {} ({e}).", dir.display()),
        }
    }
}

fn bytes_written(install_path: &Path, patch_dir: &Path, full: bool) -> u64 {
    if full {
        fs_util::directory_size(install_path).saturating_sub(fs_util::directory_size(patch_dir))
    } else {
        patch_temp_dirs(install_path)
            .iter()
            .map(|dir| fs_util::directory_size(dir))
            .sum()
    }
}

// ------------ News ------------
// The official launcher reads two small feeds from the CDN: notices (title + link) and head images (the banner carousel). Each entry carries
// every language and an end time. The site's news list fills the rest of the panel.

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn live(entry: &Value, now: i64) -> bool {
    match as_i64(entry.get("EndTimestamp")) {
        0 => true,
        end => end > now,
    }
}

fn in_language<'a>(content: Option<&'a Value>, language: &str) -> Option<&'a Value> {
    content?
        .as_array()?
        .iter()
        .find(|c| as_str(c.get("language")).eq_ignore_ascii_case(language))
}

fn secure(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with("https://") {
        return Some(url.to_string());
    }
    let rest = url.strip_prefix("http://")?;
    let host = rest.split('/').next().unwrap_or("");
    host.ends_with(".dna-panstudio.com").then(|| format!("https://{rest}"))
}

fn valid_link(url: &str) -> Option<&str> {
    let url = url.trim();
    (url.starts_with("https://") && url.matches("://").count() == 1).then_some(url)
}

pub fn notices_from(feed: &Value, language: &str, now: i64) -> Vec<Value> {
    let mut rows: Vec<(i64, Value)> = feed
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, entry)| live(entry, now))
        .filter_map(|(_, entry)| {
            let event = entry.get("event")?;
            let content = in_language(event.get("content"), language)?;
            let title = as_str(content.get("title")).trim();
            if title.is_empty() {
                return None;
            }
            let link = valid_link(as_str(content.get("titleUrl"))).unwrap_or("");
            let at = as_i64(event.get("date"));
            Some((at, article(title, link, local_month_day(at))))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, row)| row).take(MAX_ITEMS).collect()
}

pub fn slides_from(feed: &Value, language: &str, now: i64) -> Vec<Value> {
    let mut entries: Vec<(i64, &Value)> = feed
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, entry)| live(entry, now))
        .map(|(_, entry)| (as_i64(entry.get("_incrementId")), entry))
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    let mut seen: Vec<String> = Vec::new();
    let mut slides = Vec::new();
    for (_, entry) in entries {
        let Some(content) = in_language(entry.get("content"), language) else { continue };
        for image in content.get("headImages").and_then(Value::as_array).into_iter().flatten() {
            let Some(url) = secure(as_str(image.get("image"))) else { continue };
            if seen.contains(&url) {
                continue;
            }
            let jump = valid_link(as_str(image.get("imageURL"))).unwrap_or("").to_string();
            slides.push(slide(&url, &jump));
            seen.push(url);
            if slides.len() == MAX_SLIDES {
                return slides;
            }
        }
    }
    slides
}

pub fn site_news_from(body: &Value, site: &str) -> Vec<Value> {
    if as_i64(body.get("code")) != 0 {
        return Vec::new();
    }
    let site = site.trim_end_matches('/');
    let mut rows: Vec<(i64, Value)> = body
        .pointer("/data/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|post| {
            let title = as_str(post.get("title")).trim();
            let id = as_str(post.get("id")).trim();
            if title.is_empty() || id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let at = as_i64(post.get("online_at")).max(as_i64(post.get("created_at")));
            Some((at, article(title, format!("{site}/en/#/news/content?id={id}"), local_month_day(at))))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, row)| row).take(MAX_ITEMS).collect()
}

async fn fetch_site_news(profile: &Value) -> Result<Value, String> {
    let site = profile_str(profile, "dnaSiteUrl").trim_end_matches('/');
    let category = profile_str(profile, "dnaNewsCategory");
    if !site.starts_with("https://") || category.is_empty() || !category.chars().all(|c| c.is_ascii_digit()) {
        return Err("profile is missing the news site settings".to_string());
    }
    let url = format!("{site}/api/common/get-list.html");
    let response = http::client()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(format!("id={category}&pageSize={MAX_ITEMS}&page=1"))
        .send()
        .await
        .map_err(|e| format!("Request error: {}", http::describe(&e)))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status} for {url}"));
    }
    let bytes = http::read_capped(response, &url).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid JSON from {url}: {e}"))
}

pub async fn get_news(profile: &Value) -> Result<Value, String> {
    let language = match profile_str(profile, "dnaNewsLanguage") {
        "" => "EN",
        language => language,
    };
    let (notice_feed, image_feed, site) = futures::join!(
        get_json_from_any(profile, "OperationLauncherNotice/OperationLauncherNoticeProductionGlobalonline.json"),
        get_json_from_any(profile, "OperationLauncherHeadImage/OperationLauncherHeadImageProductionGlobalonline.json"),
        fetch_site_news(profile),
    );
    let now = now_secs();
    let notices = notice_feed.as_ref().map(|f| notices_from(f, language, now)).unwrap_or_default();
    let slides = image_feed.as_ref().map(|f| slides_from(f, language, now)).unwrap_or_default();
    let news = match &site {
        Ok(body) => site_news_from(body, profile_str(profile, "dnaSiteUrl")),
        Err(e) => {
            log::warn!("news: Duet Night Abyss site list failed ({e})");
            Vec::new()
        }
    };
    if notices.is_empty() && news.is_empty() && slides.is_empty() {
        return Err(notice_feed
            .err()
            .or(image_feed.err())
            .or(site.err())
            .unwrap_or_else(|| "the Duet Night Abyss feeds were empty".to_string()));
    }
    Ok(envelope(notices, news, slides))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build() -> Build {
        Build { latest: 16001, number: "1.6".into(), min_supported: 15001 }
    }

    #[test]
    fn the_manifest_is_read_and_its_version_must_match_its_folder() {
        let body = json!({ "latest_version": "16001", "latest_version_number": "1.6", "min_supported_version": "15001" });
        assert_eq!(build_from(&body).unwrap(), build());
        let mismatched = json!({ "latest_version": "17001", "latest_version_number": "1.6" });
        assert!(build_from(&mismatched).is_err());
        let malformed = json!({ "latest_version": "16001", "latest_version_number": "1.6/../x" });
        assert!(build_from(&malformed).is_err());
        assert!(build_from(&json!({ "latest_version_number": "1.6" })).is_err());
    }

    #[test]
    fn full_and_diff_archives_live_in_their_build_folder() {
        assert_eq!(package_dir(&build(), None), "1.6/16001/full_16001");
        assert_eq!(package_dir(&build(), Some(15001)), "1.6/16001/15001_16001");
    }

    #[test]
    fn the_archive_record_becomes_a_package_on_every_mirror() {
        let body = json!({
            "hdiff_file": { "name": "full_16001.hdiff", "md5": "1E60CDB01710F9F973ED14B50E0246BC", "size": 29723552214u64 },
            "new_size": 32637177238u64
        });
        let folders = vec!["https://a.example/p/1.6/16001/full_16001".to_string(), "https://b.example/p/1.6/16001/full_16001".to_string()];
        let package = package_from(&body, 16001, true, &folders).unwrap();
        assert_eq!(package.md5, "1e60cdb01710f9f973ed14b50e0246bc");
        assert_eq!(package.download_bytes, 29723552214);
        assert_eq!(package.install_bytes, 32637177238);
        assert_eq!(package.urls[1], "https://b.example/p/1.6/16001/full_16001/full_16001.hdiff");

        let escaping = json!({ "hdiff_file": { "name": "../evil.hdiff", "md5": "1e60cdb01710f9f973ed14b50e0246bc", "size": 1 } });
        assert!(package_from(&escaping, 1, true, &folders).is_err());
        let no_md5 = json!({ "hdiff_file": { "name": "full_1.hdiff", "md5": "nope", "size": 1 } });
        assert!(package_from(&no_md5, 1, true, &folders).is_err());
    }

    #[test]
    fn only_https_mirrors_are_used() {
        let profile = json!({ "dnaCdns": ["https://a.example/", "http://b.example", " https://c.example "] });
        assert_eq!(cdns(&profile), vec!["https://a.example".to_string(), "https://c.example".to_string()]);
    }

    #[test]
    fn the_version_record_round_trips_in_the_official_format() {
        let dir = std::env::temp_dir().join(format!("peebify-dna-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_installed_version(&dir, 16001).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(VERSION_FILE)).unwrap(), "{\n    \"version\": 16001\n}");
        assert_eq!(installed_version(&dir), Some(16001));
        std::fs::write(dir.join(VERSION_FILE), "{\"version\":\"oops\"}").unwrap();
        assert_eq!(installed_version(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_list_keeps_only_safe_paths_with_real_hashes_and_takes_their_sizes_from_disk() {
        let dir = std::env::temp_dir().join(format!("peebify-dna-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("EM")).unwrap();
        std::fs::write(dir.join("EM.exe"), b"hello").unwrap();
        let body = json!({
            "/EM.exe": { "md5": "7E06F6AFFE27BA1B531A80503ADCA722" },
            "/EM/Binaries/Win64/EM-Win64-Shipping.exe": { "md5": "3ef374f8f7eb44d211bcc0c063a520d9" },
            "/../escape.dll": { "md5": "3ef374f8f7eb44d211bcc0c063a520d9" },
            "/broken.bin": { "md5": "zz" },
        });
        let files = files_from(&body, &dir);
        assert_eq!(files.len(), 2);
        assert_eq!((&*files[0].path, files[0].size), ("EM.exe", 5));
        assert_eq!(files[0].hash.expected(), "7e06f6affe27ba1b531a80503adca722");
        assert_eq!(files[1].size, 0, "a missing file has no size to go by");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_noted_as_whole_at_its_size_is_not_hashed_again() {
        let dir = std::env::temp_dir().join(format!("peebify-dna-note-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(PATCH_DIR)).unwrap();
        let part = Entry::new("PeebifyPatch/full_1.hdiff", 5, Hash::parse("5d41402abc4b2a76b9719d911017c592"), "");
        let archive = dir.join(PATCH_DIR).join("full_1.hdiff");
        let trusted = |part: &Entry| Dna.triage(&dir, std::slice::from_ref(part)).trusted.len();
        std::fs::write(&archive, b"hello").unwrap();
        assert_eq!(trusted(&part), 0, "nothing vouches for it yet");
        std::fs::write(with_suffix(&archive, VERIFIED_SUFFIX), "5d41402abc4b2a76b9719d911017c592").unwrap();
        assert_eq!(trusted(&part), 1);
        std::fs::write(&archive, b"hello!").unwrap();
        assert_eq!(trusted(&part), 0, "a file of another size is checked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_hdiff_archives_are_packages_of_other_builds() {
        let unpack = Hpatch { hpatchz: PathBuf::new(), md5: String::new(), bytes: 0, full: true };
        assert!(unpack.is_package("full_15001.hdiff") && !unpack.is_package("EM.exe"));
    }

    #[test]
    fn notices_and_banners_come_from_the_launcher_feeds_in_english_and_unexpired() {
        let notices = json!({
            "a": { "EndTimestamp": 2054217599, "event": { "date": "1770048000", "content": [
                { "language": "CN", "title": "中文", "titleUrl": "https://x.example/cn" },
                { "language": "EN", "title": "Launcher FAQ", "titleUrl": "https://duetnightabyss.dna-panstudio.com/en/#/news/content?id=8673" }
            ]}},
            "b": { "EndTimestamp": 1, "event": { "date": "1770048001", "content": [{ "language": "EN", "title": "Expired" }] }},
        });
        let rows = notices_from(&notices, "EN", 1790000000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["content"], "Launcher FAQ");

        let images = json!({
            "x": { "EndTimestamp": 1792425600, "_incrementId": 56, "content": [
                { "language": "CN", "headImages": [{ "image": "http://pan01-cdn-ali-jp.dna-panstudio.com/upload/cn", "imageURL": "https://wwhttps://www.youtube.com/x" }] },
                { "language": "EN", "headImages": [
                    { "image": "http://pan01-cdn-ali-jp.dna-panstudio.com/upload/en", "imageURL": "https://www.youtube.com/watch?v=ACQ9Wt7UWmU" },
                    { "image": "http://elsewhere.example/upload/x", "imageURL": "https://www.youtube.com/x" },
                    { "image": "http://pan01-cdn-ali-jp.dna-panstudio.com/upload/broken-link", "imageURL": "https://wwhttps://www.youtube.com/x" }
                ]}
            ]},
        });
        let slides = slides_from(&images, "EN", 1790000000);
        assert_eq!(slides.len(), 2);
        assert_eq!(slides[0]["url"], "https://pan01-cdn-ali-jp.dna-panstudio.com/upload/en");
        assert_eq!(slides[0]["jumpUrl"], "https://www.youtube.com/watch?v=ACQ9Wt7UWmU");
        assert_eq!(slides[1]["jumpUrl"], "");
    }

    #[test]
    fn site_posts_become_articles_newest_first() {
        let body = json!({ "code": 0, "data": { "data": [
            { "id": "9143", "title": " The Firmament Unbound | Update Notes ", "online_at": "1775209800", "created_at": "1775209800" },
            { "id": "9149", "title": "Update Preview", "online_at": "1775361000", "created_at": "1775197200" },
            { "id": "../x", "title": "Bad id", "online_at": "1" },
        ]}});
        let rows = site_news_from(&body, "https://duetnightabyss.dna-panstudio.com/");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["content"], "Update Preview");
        assert_eq!(rows[0]["jumpUrl"], "https://duetnightabyss.dna-panstudio.com/en/#/news/content?id=9149");
        assert!(site_news_from(&json!({ "code": 10002 }), "https://x").is_empty());
    }
}
