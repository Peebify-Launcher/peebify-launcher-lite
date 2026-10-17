// ------------ Girls' Frontline 2 ------------
// Girls' Frontline 2: Exilium ships its own launcher config and file list, so this reads those and turns them into the
// manifest the engine installs. Its install record lets an update remove old asset bundles without touching files the
// game downloaded by itself. It also holds the game's Version.txt, client unpacking, client repair and install preview.
// ------------ Girls' Frontline 2 ------------
// Girls' Frontline 2: Exilium ships its own launcher config and file list, so this reads those and turns them into the
// manifest the engine installs. Its install record lets an update remove old asset bundles without touching files the
// game downloaded by itself. It also holds the game's Version.txt, the client archive and the install preview.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use peebify_transfer::net::url_host;
use peebify_transfer::part::discard_part;
use peebify_transfer::Sealed;
use serde_json::{json, Value};

use super::engine::{combine_url, merge_parts, JobCore, JobKind, Preview};
use super::extract::resolve_7z_binary;
use super::fs_util::manifest_key;
use super::manifest::{self, Container, Cx, Hash, Manifest, Pack, Policy, Publisher};
use super::validator::{recorded, Record, Triage};
use super::{game_profiles, http, members};

pub const AB_DIR_PARTS: [&str; 4] = [
    "GF2_Exilium_Data",
    "LocalCache",
    "Data",
    "AssetBundles_Windows",
];
const AB_REMOTE_DIR: &str = "AssetBundles_Windows";
const AB_DEST_PREFIX: &str = "GF2_Exilium_Data/LocalCache/Data/AssetBundles_Windows/";
const LOCAL_VERSION_FILE: &str = "Version.txt";
const CLIENT_MARKER_FILE: &str = "GF2_Exilium_Data/LocalCache/.peebify-gf2.json";
const CONFIG_CACHE_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Config {
    pub client_version: String,
    pub client_url: String,
    pub ab_version: String,
    pub ab_base: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub md5: String,
    pub size: u64,
    pub bucket: String,
    pub voice: Option<String>,
}

impl Entry {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.trim().split('#');
        let path = fields.next()?;
        let md5 = fields.next()?;
        let size = fields.next()?.trim().parse().ok()?;
        let bucket = fields.next()?.trim();
        if md5.len() != 32 || bucket.is_empty() {
            return None;
        }
        let name = path.rsplit('/').next()?.trim();
        if name.is_empty() || name.contains('\\') {
            return None;
        }
        Some(Self {
            voice: voice_language(name),
            name: name.to_string(),
            md5: md5.trim().to_lowercase(),
            size,
            bucket: bucket.to_string(),
        })
    }

    fn manifest_line(&self) -> String {
        format!(
            "{{0}}/{}#{}#{}#{}",
            self.name,
            self.md5.to_uppercase(),
            self.size,
            self.bucket
        )
    }

    fn entry(&self, ab_base: &str) -> manifest::Entry {
        let url = combine_url(ab_base, &format!("{AB_REMOTE_DIR}/{}/{}", self.bucket, self.name));
        manifest::Entry::new(format!("{AB_DEST_PREFIX}{}", self.name), self.size, Hash::parse(&self.md5), url).live()
    }
}

fn voice_language(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    if stem.len() != 34 {
        return None;
    }
    let (Some(prefix), Some(hash)) = (stem.get(..2), stem.get(2..)) else {
        return None;
    };
    let looks_like_pack = prefix.chars().all(|c| c.is_ascii_uppercase())
        && hash.chars().all(|c| c.is_ascii_hexdigit());
    looks_like_pack.then(|| prefix.to_string())
}

fn config_url(profile: &Value) -> Result<&str, String> {
    profile
        .get("gf2ConfigUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "no gf2ConfigUrl for profile".to_string())
}

fn client_package(profile: &Value) -> &str {
    profile
        .get("gf2ClientPackage")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("GF2_Exilium_Origin.zip")
}

fn plug_config(response: &Value) -> Result<Value, String> {
    let raw = response
        .pointer("/data/flexible_config/nexon_plug")
        .and_then(|v| v.as_str())
        .ok_or("GF2 launcher config has no nexon_plug section")?;
    serde_json::from_str(raw).map_err(|e| format!("Invalid GF2 nexon_plug payload: {e}"))
}

fn plug_string(plug: &Value, key: &str) -> Result<String, String> {
    plug.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("GF2 launcher config has no {key}"))
}

fn ab_bucket(ab_version: &str) -> &str {
    ab_version.rsplit('.').next().unwrap_or(ab_version)
}

pub fn parse_manifest(text: &str) -> Vec<Entry> {
    text.lines().filter_map(Entry::parse).collect()
}

type ConfigCache = Mutex<Option<(Value, Instant)>>;

fn config_cache() -> &'static ConfigCache {
    static CACHE: OnceLock<ConfigCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

async fn fetch_plug(profile: &Value, fresh: bool) -> Result<Value, String> {
    if !fresh {
        if let Some((plug, at)) = config_cache().lock().as_ref() {
            if at.elapsed() < CONFIG_CACHE_TIMEOUT {
                return Ok(plug.clone());
            }
        }
    }
    let response = http::get_json(config_url(profile)?).await?;
    let plug = plug_config(&response)?;
    *config_cache().lock() = Some((plug.clone(), Instant::now()));
    Ok(plug)
}

pub struct ClientInfo {
    pub version: String,
    pub url: String,
    pub file: String,
}

pub async fn fetch_client_info(profile: &Value) -> Result<ClientInfo, String> {
    let plug = fetch_plug(profile, false).await?;
    let version = plug_string(&plug, "game_client_version")?;
    let base = plug_string(&plug, "game_download_addr")?;
    let file = client_package(profile).to_string();
    Ok(ClientInfo {
        url: combine_url(&base, &format!("{version}/{file}")),
        version,
        file,
    })
}

pub struct Versions {
    pub client_version: String,
    pub ab_version: String,
}

pub async fn fetch_versions_with(profile: &Value, fresh: bool) -> Result<Versions, String> {
    let plug = fetch_plug(profile, fresh).await?;
    Ok(Versions {
        client_version: plug_string(&plug, "game_client_version")?,
        ab_version: plug_string(&plug, "ab_resource_version")?,
    })
}

pub async fn fetch_config(profile: &Value) -> Result<Config, String> {
    let plug = fetch_plug(profile, false).await?;
    let client = fetch_client_info(profile).await?;
    let ab_version = plug_string(&plug, "ab_resource_version")?;
    let ab_base = plug_string(&plug, "ab_resource_addr")?;

    let manifest_url = combine_url(
        &ab_base,
        &format!(
            "{AB_REMOTE_DIR}/{}/{LOCAL_VERSION_FILE}",
            ab_bucket(&ab_version)
        ),
    );
    let entries = parse_manifest(&http::get_text(&manifest_url).await?);
    if entries.is_empty() {
        return Err(format!(
            "GF2 resource manifest at {manifest_url} listed no files."
        ));
    }

    Ok(Config {
        client_url: client.url,
        client_version: client.version,
        ab_version,
        ab_base,
        entries,
    })
}

impl Config {
    pub fn base_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.voice.is_none())
    }

    pub fn entries(&self) -> Vec<manifest::Entry> {
        self.base_entries().map(|e| e.entry(&self.ab_base)).collect()
    }

    pub fn download_bytes(&self) -> u64 {
        self.base_entries().map(|e| e.size).sum()
    }

    pub fn file_count(&self) -> u64 {
        self.base_entries().count() as u64
    }
}

pub fn ab_dir(install_path: &Path) -> PathBuf {
    AB_DIR_PARTS
        .iter()
        .fold(install_path.to_path_buf(), |p, part| p.join(part))
}

pub fn is_bundle(path: &str) -> bool {
    path.starts_with(AB_DEST_PREFIX)
}

pub fn client_version_from_url(url: &str) -> Option<&str> {
    url.rsplit('/').nth(1).filter(|v| !v.is_empty())
}

pub fn unchanged_bundles(install_path: &Path, files: &[manifest::Entry]) -> HashSet<String> {
    let dir = ab_dir(install_path);
    let recorded: HashMap<String, Entry> =
        recorded_entries(&dir).into_iter().map(|e| (e.name.clone(), e)).collect();
    files
        .iter()
        .filter(|f| is_bundle(&f.path))
        .filter_map(|f| {
            let name = f.path.rsplit('/').next()?;
            let entry = recorded.get(name)?;
            let matches = entry.size == f.size
                && entry.md5.eq_ignore_ascii_case(&f.hash.expected())
                && std::fs::metadata(dir.join(name)).is_ok_and(|m| m.len() == f.size);
            matches.then(|| manifest_key(&f.path))
        })
        .collect()
}

fn version_line(file: &manifest::Entry) -> Option<Entry> {
    let name = file.path.rsplit('/').next()?.to_string();
    let bucket = file
        .url()
        .rsplit('/')
        .nth(1)
        .filter(|b| !b.is_empty() && *b != AB_REMOTE_DIR)?;
    Some(Entry {
        voice: voice_language(&name),
        name,
        md5: file.hash.expected(),
        size: file.size,
        bucket: bucket.to_string(),
    })
}

pub fn write_local_manifest(install_path: &Path, files: &[manifest::Entry]) -> Result<usize, String> {
    let dir = ab_dir(install_path);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut body = String::new();
    let mut written: HashSet<String> = HashSet::new();
    for entry in files.iter().filter_map(version_line) {
        body.push_str(&entry.manifest_line());
        body.push('\n');
        written.insert(entry.name);
    }
    let mut carried = 0usize;
    for entry in recorded_entries(&dir) {
        if !written.contains(&entry.name)
            && std::fs::metadata(dir.join(&entry.name)).is_ok_and(|m| m.len() == entry.size)
        {
            body.push_str(&entry.manifest_line());
            body.push('\n');
            written.insert(entry.name);
            carried += 1;
        }
    }

    let path = dir.join(LOCAL_VERSION_FILE);
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    super::fs_util::finalize_replace(&tmp, &path)?;
    Ok(carried)
}

fn recorded_entries(ab_dir: &Path) -> Vec<Entry> {
    std::fs::read_to_string(ab_dir.join(LOCAL_VERSION_FILE))
        .map(|text| parse_manifest(&text))
        .unwrap_or_default()
}

pub fn installed_client_version(install_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(install_path.join(CLIENT_MARKER_FILE)).ok()?;
    let marker: Value = serde_json::from_str(&text).ok()?;
    marker
        .get("clientVersion")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn record_client_version(_sealed: &peebify_transfer::Sealed, install_path: &Path, version: &str) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&json!({ "clientVersion": version }))
        .map_err(|e| e.to_string())?;
    super::fs_util::write_atomic(&install_path.join(CLIENT_MARKER_FILE), text.as_bytes())
}

pub fn client_update_pending(install_path: &Path, client_version: &str) -> bool {
    installed_client_version(install_path).is_some_and(|v| v != client_version)
}

pub fn client_is_current(install_path: &Path, exe_name: &str, client_version: &str) -> bool {
    install_path.join(exe_name).is_file()
        && installed_client_version(install_path).as_deref() == Some(client_version)
}

// ------------ Install Preview ------------
// The client package and the resources to download.
pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    let config = fetch_config(profile).await?;
    let client_bytes = http::content_length(&config.client_url).await.unwrap_or(0);
    let resource_bytes = config.download_bytes();

    let entries = vec![
        (
            "client".to_string(),
            format!("game client {}", config.client_version),
            client_bytes,
            1,
        ),
        (
            "resources".to_string(),
            "game resources".to_string(),
            resource_bytes,
            config.file_count(),
        ),
    ];

    Ok(Preview {
        version: config.ab_version.clone(),
        download_bytes: client_bytes + resource_bytes,
        install_bytes: 0,
        file_count: config.file_count() + 1,
        approximate: true,
        parts: merge_parts(&entries),
    })
}

// ------------ Install ------------
// The resource bundles are the manifest. The game adds bundles of its own and records them in the same Version.txt,
// so the bundles it vouches for skip their checksum, and its lines are kept when Peebify rewrites the file. The client
// is an archive that comes with the resources when it is out of date; its files are read from the archive's index so the
// install record covers them, and a repair checks them against that record and takes the broken ones from the archive.
pub(super) struct Gf2;

impl Publisher for Gf2 {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let config = fetch_config(cx.profile).await?;
            let mut manifest = Manifest { entries: config.entries(), mirrors: vec![config.ab_base], ..Default::default() };
            log::info!(
                "{} resources {} resolved with {} files.",
                game_profiles::display_name(cx.profile),
                config.ab_version,
                manifest.entries.len()
            );
            manifest.version = config.ab_version;
            let kept = match cx.purpose {
                JobKind::Verify => Vec::new(),
                _ => recorded_client(cx.dir).await,
            };
            match cx.purpose {
                JobKind::Install => match client_archive(cx).await? {
                    Some((container, files)) => {
                        manifest.containers.push(container);
                        manifest.listed = if files.is_empty() { kept } else { files };
                    }
                    None => manifest.listed = kept,
                },
                _ => manifest.entries.extend(kept),
            }
            Ok(manifest)
        })
    }

    fn latest<'a>(&'a self, profile: &'a Value, fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let versions = fetch_versions_with(profile, fresh).await?;
            Ok(json!({ "version": versions.ab_version, "clientVersion": versions.client_version }))
        })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn update_pending(&self, latest: &Value, dir: &Path) -> bool {
        latest["clientVersion"].as_str().filter(|v| !v.is_empty()).is_some_and(|client| client_update_pending(dir, client))
    }

    fn policy(&self, dir: &Path) -> Policy {
        let unrecorded = super::game_manager::local_game_version_at(dir).is_none();
        Policy { checksums: unrecorded, records: unrecorded, partial_verify: true, ..Policy::default() }
    }

    fn triage(&self, dir: &Path, files: &[manifest::Entry]) -> Triage {
        let mut triage = Record::load(dir).triage(dir, files);
        triage.trusted.extend(unchanged_bundles(dir, files));
        triage
    }

    fn after_seal<'a>(
        &'a self,
        job: &'a Arc<JobCore>,
        dir: &'a Path,
        manifest: &'a Manifest,
        sealed: &'a Sealed,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            if let Some(client) = manifest.containers.first().and_then(|c| c.parts.first()) {
                note_client(job.profile(), dir, client, sealed)?;
            }
            let bundles = manifest.entries.clone();
            let recorded = bundles.len();
            let path = dir.to_path_buf();
            let carried = tauri::async_runtime::spawn_blocking(move || write_local_manifest(&path, &bundles))
                .await
                .map_err(|e| format!("GF2 bundle finish task panicked: {e}"))??;
            log::info!(
                "GF2: recorded {recorded} resources in the install's Version.txt for the game to read ({carried} line(s) the game added kept)."
            );
            super::validator::update_game_config_file(sealed, dir, &manifest.version)?;
            Ok(true)
        })
    }
}

async fn recorded_client(dir: &Path) -> Vec<manifest::Entry> {
    let record = recorded(dir).await;
    let packs: Arc<[Pack]> = record.packs.clone().into();
    let is_member = |file: &manifest::Entry| matches!(file.hash, Hash::Crc32(_));
    record.entries().into_iter().filter(is_member).map(|file| file.member_of(&packs)).collect()
}

async fn client_archive(cx: &Cx<'_>) -> Result<Option<(Container, Vec<manifest::Entry>)>, String> {
    let client = fetch_client_info(cx.profile).await?;

    if client_is_current(cx.dir, game_profiles::executable_name(cx.profile), &client.version) {
        log::info!(
            "GF2 client {} already installed — only resources need checking.",
            client.version
        );
        return Ok(None);
    }

    resolve_7z_binary(cx.app).await?;
    let size = http::content_length(&client.url)
        .await
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            log::warn!(
                "GF2 client {}: {} gave no size for the package.",
                client.version,
                url_host(&client.url)
            );
            "Could not read the size of the GF2 client package. Try again in a moment.".to_string()
        })?;
    log::info!("GF2 client {} queued with the resources ({size} bytes).", client.version);
    let packs: Arc<[Pack]> = vec![Pack { url: client.url.as_str().into(), size }].into();
    let files = members::listing(&packs, &cx.job).await.unwrap_or_else(|e| {
        log::warn!("GF2: could not read the client archive's index, so a repair will not cover the client: {e}");
        Vec::new()
    });
    let archive = manifest::Entry::new(client.file, size, Hash::None, client.url);
    Ok(Some((Container { parts: vec![archive], ..Default::default() }, files)))
}

fn note_client(profile: &Value, install_path: &Path, client: &manifest::Entry, sealed: &Sealed) -> Result<(), String> {
    let url = client.url();
    let version = client_version_from_url(url)
        .ok_or_else(|| format!("Could not read the GF2 client version from {url}"))?;
    let exe = game_profiles::executable_name(profile);
    if !install_path.join(exe).is_file() {
        let archive = super::fs_util::safe_join(install_path, &client.path)?;
        let _ = std::fs::remove_file(&archive);
        discard_part(&archive);
        return Err(format!(
            "The GF2 client package unpacked without {exe}. The download may be incomplete. Try installing again."
        ));
    }
    log::info!("GF2 client {version} unpacked.");
    record_client_version(sealed, install_path, version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;

    fn bundle(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let folder = ab_dir(dir.path());
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    const MD5: &str = "0123456789abcdef0123456789abcdef";

    fn res(name: &str, size: u64) -> manifest::Entry {
        let url = format!("https://cdn.example/prod/{AB_REMOTE_DIR}/27767/{name}");
        manifest::Entry::new(format!("{AB_DEST_PREFIX}{name}"), size, Hash::parse(MD5), url).live()
    }

    #[test]
    fn a_voice_pack_name_is_recognised_and_a_non_ascii_name_does_not_panic() {
        assert_eq!(
            voice_language("JA0123456789abcdef0123456789abcdef.bundle").as_deref(),
            Some("JA")
        );
        assert_eq!(voice_language("0123456789abcdef0123456789abcdef.bundle"), None);
        let name = format!("\u{4e2d}{}.bundle", "a".repeat(31));
        assert_eq!(voice_language(&name), None);
    }

    #[test]
    fn a_client_update_is_pending_only_when_the_recorded_version_differs() {
        let dir = TempDir::new("client-pending");
        assert!(!client_update_pending(dir.path(), "1.2.0"));

        record_client_version(&peebify_transfer::Sealed::nothing_written(), dir.path(), "1.1.0").unwrap();
        assert!(client_update_pending(dir.path(), "1.2.0"));
        assert!(!client_update_pending(dir.path(), "1.1.0"));
    }

    #[test]
    fn an_install_with_no_version_file_is_checked_by_checksum_and_recorded() {
        let dir = TempDir::new("gf2-policy");
        let policy = Gf2.policy(dir.path());
        assert!(policy.checksums && policy.records && policy.partial_verify, "a Steam copy learns its version from its first repair");
        let config = dir.path().join(crate::backend::validator::GAME_CONFIG_FILE);
        std::fs::write(config, r#"{"version":"1.0.0"}"#).unwrap();
        let policy = Gf2.policy(dir.path());
        assert!(!policy.checksums && !policy.records && policy.partial_verify);
    }

    #[test]
    fn the_games_own_version_file_vouches_only_for_bundles_it_records_unchanged() {
        let dir = TempDir::new("game-vouches");
        bundle(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle", b"data");
        bundle(&dir, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle", b"data");
        bundle(&dir, "cccccccccccccccccccccccccccccccc.bundle", b"data");
        std::fs::write(
            ab_dir(dir.path()).join(LOCAL_VERSION_FILE),
            "{0}/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle#0123456789ABCDEF0123456789ABCDEF#4#27767\n\
             {0}/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle#FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF#4#27767\n",
        )
        .unwrap();
        let files = [
            res("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle", 4),
            res("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle", 4),
            res("cccccccccccccccccccccccccccccccc.bundle", 4),
        ];
        let trusted = unchanged_bundles(dir.path(), &files);
        assert_eq!(trusted, HashSet::from([manifest_key(&files[0].path)]));
        assert!(files.iter().all(|f| f.live), "a bundle replaces one the game reads");
    }

    #[tokio::test]
    async fn the_clients_files_are_the_recorded_ones_with_a_crc32() {
        let dir = TempDir::new("client-files");
        dir.write("GF2.exe", b"exe");
        dir.write("GF2_Exilium_Data/data.bin", b"data");
        bundle(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle", b"data");
        let packs: Arc<[Pack]> = vec![Pack { url: "https://cdn.example/1.2.0/GF2_Exilium_Origin.zip".into(), size: 7 }].into();
        let client = |path: &str, data: &[u8]| {
            manifest::Entry::new(path, data.len() as u64, Hash::Crc32(crc32fast::hash(data)), "").member_of(&packs)
        };
        let files = [client("GF2.exe", b"exe"), client("GF2_Exilium_Data/data.bin", b"data"), res("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle", 4)];
        Record::of(dir.path(), "27767", &files).save(dir.path()).unwrap();

        let kept = recorded_client(dir.path()).await;

        let paths: Vec<&str> = kept.iter().map(|f| &*f.path).collect();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"GF2.exe") && paths.contains(&"GF2_Exilium_Data/data.bin"), "{paths:?}");
        assert!(kept.iter().all(|f| matches!(&f.source, manifest::Source::Member { packs: p, at: None } if p[0].size == 7)));
    }

    #[test]
    fn the_rewritten_manifest_keeps_lines_the_game_added() {
        let dir = TempDir::new("carry-over");
        bundle(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle", b"data");
        bundle(&dir, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle", b"data");
        bundle(&dir, "CN000000000000000000000000000000ff.gf", b"data");
        bundle(&dir, "dddddddddddddddddddddddddddddddd.bundle", b"data");
        std::fs::write(
            ab_dir(dir.path()).join(LOCAL_VERSION_FILE),
            "{0}/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle#0123456789ABCDEF0123456789ABCDEF#4#27767\n\
             {0}/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle#0123456789ABCDEF0123456789ABCDEF#4#27767\n\
             {0}/CN000000000000000000000000000000ff.gf#0123456789ABCDEF0123456789ABCDEF#4#27767\n\
             {0}/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.bundle#0123456789ABCDEF0123456789ABCDEF#4#27767\n",
        )
        .unwrap();

        let carried = write_local_manifest(
            dir.path(),
            &[res("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle", 4)],
        )
        .unwrap();

        let text = std::fs::read_to_string(ab_dir(dir.path()).join(LOCAL_VERSION_FILE)).unwrap();
        let recorded: Vec<String> = parse_manifest(&text).into_iter().map(|e| e.name).collect();

        assert_eq!(carried, 2);
        assert_eq!(recorded.len(), 3);
        assert!(recorded.contains(&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle".to_string()));
        assert!(
            recorded.contains(&"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle".to_string()),
            "a bundle the game fetched was dropped from Version.txt"
        );
        assert!(recorded.contains(&"CN000000000000000000000000000000ff.gf".to_string()));
        assert!(
            !recorded.contains(&"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.bundle".to_string()),
            "a line whose file is gone was carried over"
        );
        assert!(
            !recorded.contains(&"dddddddddddddddddddddddddddddddd.bundle".to_string()),
            "a file nobody recorded was invented into the manifest"
        );
    }
}
