// ------------ Brown Dust II ------------
// Brown Dust II ships as one big .tied.dat package instead of a list of files. This file works out where to get it, which version it is, and how to unpack it.
// The files of an install come from the package as it unpacks, so the install record lists them for updates and repairs; it also holds the install preview and the check of an install Peebify did not make.
use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use peebify_transfer::fsx::ContentHasher;
use peebify_transfer::Sealed;
use serde_json::{json, Value};

use super::engine::{package_preview, JobCore, JobKind, Preview};
use super::extract::Extracted;
use super::fs_util::{safe_join, CRC32_PREFIX};
use super::game_manager::local_game_version_at;
use super::game_profiles::{self, profile_str, required_str};
use super::manifest::{json_u64, origin_of, Container, Cx, Entry, Hash, Manifest, Policy, Publisher, Shape, Unpacked, Unpacker};
use super::{http, validator};

pub const SETTINGS_FILE: &str = "launcher.settings";
pub const PACKAGE_SUFFIX: &str = ".tied.dat";

const LOCAL_FILE_SIG: u32 = 0x0403_4b50;
const CENTRAL_DIR_SIG: u32 = 0x0201_4b50;
const MAX_EXTENDS: usize = 4;
const ZIP64_MARKER: u32 = 0xFFFF_FFFF;
const COPY_BUFFER: usize = 1 << 20;
const PROGRESS_STEP: u64 = 8 << 20;
const DAMAGED: &str = "The download is damaged. Peebify downloads it again when you retry.";

pub fn is_damaged_package_error(message: &str) -> bool {
    message.contains(DAMAGED)
}

fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn merge_into(base: &mut Value, overlay: Value) {
    let Value::Object(overlay) = overlay else {
        return;
    };
    let Value::Object(target) = base else {
        return;
    };
    for (key, value) in overlay {
        match (target.get_mut(&key), value) {
            (Some(existing), value @ Value::Object(_)) if existing.is_object() => {
                merge_into(existing, value);
            }
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
}

// ------------ Package Lookup ------------
// Asks Neowiz's starter config for this game's download manifest and turns it into the package URL, size and checksum to download.
pub async fn fetch_game_manifest(profile: &Value) -> Result<Value, String> {
    let starter = http::get_json(required_str(profile, "bdStarterConfigUrl")?).await?;
    let key = required_str(profile, "bdGameKey")?;

    let entry = starter
        .get("game_config_urls")
        .and_then(|map| map.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            starter
                .get("game_config_url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|base| format!("{}/{key}.json", base.trim_end_matches('/')))
        })
        .ok_or_else(|| {
            format!("The Brown Dust II starter config lists no download manifest for game {key}.")
        })?;

    let mut chain: Vec<Value> = Vec::new();
    let mut next = Some(entry);
    while let Some(url) = next.take() {
        if chain.len() >= MAX_EXTENDS {
            return Err(format!(
                "The Brown Dust II manifest chains more than {MAX_EXTENDS} 'extends' documents; refusing to follow it further."
            ));
        }
        let doc = http::get_json(&url).await?;
        next = doc
            .get("extends")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        chain.push(doc);
    }

    let mut merged = json!({});
    for doc in chain.into_iter().rev() {
        merge_into(&mut merged, doc);
    }
    Ok(merged)
}

pub struct Package {
    pub version: String,
    pub url: String,
    pub file_name: String,
    pub download_bytes: u64,
    pub install_bytes: u64,
    pub tied_crc: u32,
    pub settings: Value,
}

pub async fn fetch_package(profile: &Value) -> Result<Package, String> {
    let manifest = fetch_game_manifest(profile).await?;

    let last = manifest
        .get("last_file")
        .filter(|v| v.is_object())
        .ok_or("The Brown Dust II manifest carries no 'last_file' package entry.")?;

    let relative = profile_str(last, "file");
    if relative.is_empty() {
        return Err("The Brown Dust II manifest's 'last_file' has no file name.".to_string());
    }
    let base = profile_str(&manifest, "download_url");
    if base.is_empty() {
        return Err("The Brown Dust II manifest has no 'download_url'.".to_string());
    }

    let url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        relative.trim_start_matches('/')
    );
    let file_name = package_file_name(relative)?;

    let version = manifest_version(&manifest)?;

    let tied_crc = u32::from_str_radix(profile_str(last, "tied_crc"), 16).map_err(|_| {
        format!(
            "The Brown Dust II manifest's tied_crc ('{}') is not a hex checksum.",
            profile_str(last, "tied_crc")
        )
    })?;

    let listed_bytes = json_u64(last.get("file_size")).unwrap_or(0);
    let download_bytes = match http::content_length(&url).await {
        Some(bytes) if bytes > 0 => bytes,
        _ => {
            let fallback = fallback_download_bytes(listed_bytes);
            log::warn!(
                "Brown Dust II: {url} reported no size, so the package is taken as the manifest's file_size ({listed_bytes}) plus the {ZIP_TRAILER_BYTES} byte zip trailer it leaves out ({fallback})."
            );
            fallback
        }
    };
    if download_bytes == 0 {
        return Err("Could not determine the size of the Brown Dust II package.".to_string());
    }

    Ok(Package {
        version,
        url,
        file_name,
        download_bytes,
        install_bytes: json_u64(last.get("unziped_size")).unwrap_or(0),
        tied_crc,
        settings: manifest.get("settings").cloned().unwrap_or(Value::Null),
    })
}

fn manifest_version(manifest: &Value) -> Result<String, String> {
    let from_file = manifest
        .get("last_file")
        .map(|last| profile_str(last, "version"))
        .unwrap_or_default();
    let version = if from_file.is_empty() {
        profile_str(manifest, "last_version")
    } else {
        from_file
    };
    if version.is_empty() {
        return Err("The Brown Dust II manifest carries no package version.".to_string());
    }
    Ok(version.to_string())
}

fn package_file_name(relative: &str) -> Result<String, String> {
    let name = relative.rsplit('/').next().unwrap_or_default();
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if plain && name.starts_with(|c: char| c.is_ascii_alphanumeric()) && !name.ends_with('.') {
        return Ok(name.to_string());
    }
    Err(format!(
        "The Brown Dust II manifest's 'last_file' names '{relative}', which is not a plain file name."
    ))
}

const ZIP_TRAILER_BYTES: u64 = 84;

fn fallback_download_bytes(listed_bytes: u64) -> u64 {
    if listed_bytes == 0 {
        0
    } else {
        listed_bytes + ZIP_TRAILER_BYTES
    }
}

// ------------ Installed Version ------------
// The install record names the version. An install the official starter made names it by the package it left behind.
fn package_version(name: &str) -> Option<&str> {
    name.strip_suffix(PACKAGE_SUFFIX)
        .filter(|version| !version.is_empty() && version.chars().all(|c| c.is_ascii_digit()))
}

fn version_key(version: &str) -> (usize, &str) {
    let trimmed = version.trim_start_matches('0');
    (trimmed.len(), trimmed)
}

pub fn installed_version(install_path: &Path) -> Option<String> {
    if let Some(recorded) = validator::Record::version_at(install_path) {
        return Some(recorded);
    }
    if install_path.join(validator::INSTALL_MARKER_FILE).exists() {
        return None;
    }
    std::fs::read_dir(install_path)
        .ok()?
        .filter_map(|entry| package_version(&entry.ok()?.file_name().to_string_lossy()).map(str::to_string))
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
}

pub fn write_launcher_settings(install_path: &Path, settings: &Value) -> Result<(), String> {
    if !settings.is_object() {
        return Err(format!(
            "The Brown Dust II manifest carries no 'settings' block, so {SETTINGS_FILE} cannot be written and the game would not know which service to sign in to."
        ));
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    super::fs_util::write_atomic(&install_path.join(SETTINGS_FILE), text.as_bytes())
}

// ------------ Package Extraction ------------
// Streams the package straight into the game folder while checking its CRC.
struct Counting<R: Read> {
    inner: R,
    hasher: ContentHasher,
    total: u64,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            self.hasher.update(&buf[..read]);
            self.total += read as u64;
        }
        Ok(read)
    }
}

fn crc_of(hasher: ContentHasher) -> u32 {
    hasher.finish().strip_prefix(CRC32_PREFIX).and_then(|crc| crc.parse().ok()).unwrap_or_default()
}

fn open_outer_stream(package: &Path) -> Result<(Box<dyn Read>, String), String> {
    let file = std::fs::File::open(package)
        .map_err(|e| format!("Could not open {}: {e}", package.display()))?;
    let mut reader = BufReader::with_capacity(COPY_BUFFER, file);

    let mut header = [0u8; 30];
    reader
        .read_exact(&mut header)
        .map_err(|e| {
            format!(
                "Could not read the package header of {} ({e}). {DAMAGED}",
                package.display()
            )
        })?;

    if le_u32(&header, 0) != LOCAL_FILE_SIG {
        return Err(format!(
            "{} does not start with a ZIP header, so it is not a Brown Dust II package. {DAMAGED}",
            package.display()
        ));
    }
    let flags = le_u16(&header, 6);
    if flags & 0x0008 != 0 {
        return Err(
            "The Brown Dust II package stores its size in a trailing data descriptor, which Peebify cannot stream. Report this: Neowiz changed their packer."
                .to_string(),
        );
    }
    let method = le_u16(&header, 8);
    let compressed = le_u32(&header, 18);
    if compressed == ZIP64_MARKER {
        return Err(
            "The Brown Dust II package uses ZIP64 sizes, which Peebify does not read yet. Report this: Neowiz changed their packer."
                .to_string(),
        );
    }
    let name_len = le_u16(&header, 26) as usize;
    let extra_len = le_u16(&header, 28) as usize;

    let mut name = vec![0u8; name_len];
    reader
        .read_exact(&mut name)
        .map_err(|e| format!("Could not read the package entry name ({e}). {DAMAGED}"))?;
    let name = String::from_utf8_lossy(&name).into_owned();
    if extra_len > 0 {
        std::io::copy(&mut reader.by_ref().take(extra_len as u64), &mut std::io::sink())
            .map_err(|e| format!("Could not skip the package extra field ({e}). {DAMAGED}"))?;
    }

    let limited = reader.take(compressed as u64);
    let stream: Box<dyn Read> = match method {
        0 => Box::new(limited),
        8 => Box::new(flate2::read::DeflateDecoder::new(limited)),
        other => {
            return Err(format!(
                "The Brown Dust II package uses compression method {other}, which Peebify does not read. Report this: Neowiz changed their packer."
            ))
        }
    };
    Ok((stream, name))
}

fn skip_entry_data(entry: &mut impl Read, name: &str) -> Result<(), String> {
    std::io::copy(entry, &mut std::io::sink())
        .map(|_| ())
        .map_err(|e| format!("Could not read past {name} ({e}). {DAMAGED}"))
}

pub fn extract_package(
    package: &Path,
    install_path: &Path,
    wanted: Option<&HashSet<String>>,
    expect_crc: u32,
    expect_bytes: u64,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64),
) -> Result<Vec<Entry>, String> {
    let (stream, inner_name) = open_outer_stream(package)?;
    log::info!(
        "Brown Dust II: unpacking {inner_name} out of {}",
        package.display()
    );

    let mut reader = BufReader::with_capacity(
        COPY_BUFFER,
        Counting {
            inner: stream,
            hasher: ContentHasher::for_expected(CRC32_PREFIX),
            total: 0,
        },
    );

    let mut files: Vec<Entry> = Vec::new();
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut written: u64 = 0;
    let mut next_tick: u64 = PROGRESS_STEP;

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("Install cancelled by user.".to_string());
        }

        let mut signature = [0u8; 4];
        reader
            .read_exact(&mut signature)
            .map_err(|e| format!("The Brown Dust II package ended early ({e}). {DAMAGED}"))?;
        let signature = u32::from_le_bytes(signature);
        if signature == CENTRAL_DIR_SIG {
            break;
        }
        if signature != LOCAL_FILE_SIG {
            return Err(format!(
                "Unexpected record 0x{signature:08X} inside the Brown Dust II package after {} file(s). {DAMAGED}",
                files.len()
            ));
        }

        let mut header = [0u8; 26];
        reader
            .read_exact(&mut header)
            .map_err(|e| format!("Could not read a package file header ({e}). {DAMAGED}"))?;

        let flags = le_u16(&header, 2);
        let method = le_u16(&header, 4);
        let crc = le_u32(&header, 10);
        let compressed = le_u32(&header, 14);
        let uncompressed = le_u32(&header, 18);
        let name_len = le_u16(&header, 22) as usize;
        let extra_len = le_u16(&header, 24) as usize;

        if flags & 0x0008 != 0 {
            return Err(
                "A file inside the Brown Dust II package uses a trailing data descriptor, which Peebify cannot stream. Report this: Neowiz changed their packer."
                    .to_string(),
            );
        }
        if compressed == ZIP64_MARKER || uncompressed == ZIP64_MARKER {
            return Err(
                "A file inside the Brown Dust II package uses ZIP64 sizes, which Peebify does not read yet. Report this: Neowiz changed their packer."
                    .to_string(),
            );
        }

        let mut name = vec![0u8; name_len];
        reader
            .read_exact(&mut name)
            .map_err(|e| format!("Could not read a package file name ({e}). {DAMAGED}"))?;
        let name = String::from_utf8_lossy(&name).into_owned();
        if extra_len > 0 {
            std::io::copy(
                &mut reader.by_ref().take(extra_len as u64),
                &mut std::io::sink(),
            )
            .map_err(|e| format!("Could not skip a package extra field ({e}). {DAMAGED}"))?;
        }

        let keep = wanted.is_none_or(|set| set.contains(&name));

        if name.ends_with('/') || name.ends_with('\\') {
            if keep {
                let dir = safe_join(install_path, &name)?;
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
            }
            skip_entry_data(&mut reader.by_ref().take(compressed as u64), &name)?;
            continue;
        }

        let mut limited = reader.by_ref().take(compressed as u64);
        let mut entry: Box<dyn Read> = match method {
            0 => Box::new(&mut limited),
            8 => Box::new(flate2::read::DeflateDecoder::new(&mut limited)),
            other => {
                return Err(format!(
                    "{name} inside the Brown Dust II package uses compression method {other}, which Peebify does not read. Report this: Neowiz changed their packer."
                ))
            }
        };

        let mut sink: Box<dyn Write> = if keep {
            let dest = safe_join(install_path, &name)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            }
            let handle = std::fs::File::create(&dest)
                .map_err(|e| format!("Could not create {}: {e}", dest.display()))?;
            Box::new(BufWriter::with_capacity(COPY_BUFFER, handle))
        } else {
            Box::new(std::io::sink())
        };

        let mut hasher = ContentHasher::for_expected(CRC32_PREFIX);
        let mut remaining = uncompressed as u64;
        while remaining > 0 {
            if cancel.load(Ordering::SeqCst) {
                return Err("Install cancelled by user.".to_string());
            }
            let want = remaining.min(buffer.len() as u64) as usize;
            entry
                .read_exact(&mut buffer[..want])
                .map_err(|e| format!("Could not unpack {name} ({e}). {DAMAGED}"))?;
            hasher.update(&buffer[..want]);
            sink.write_all(&buffer[..want])
                .map_err(|e| format!("Could not write {name}: {e}"))?;
            remaining -= want as u64;
            written += want as u64;
            if written >= next_tick {
                on_progress(written);
                next_tick = written + PROGRESS_STEP;
            }
        }
        drop(entry);
        skip_entry_data(&mut limited, &name)?;
        sink.flush()
            .map_err(|e| format!("Could not finish writing {name}: {e}"))?;

        let actual = crc_of(hasher);
        if actual != crc {
            return Err(format!(
                "{name} failed its checksum after unpacking (expected {crc:08X}, got {actual:08X}). {DAMAGED}"
            ));
        }

        files.push(Entry::new(name, uncompressed as u64, Hash::Crc32(crc), ""));
    }

    std::io::copy(&mut reader, &mut std::io::sink())
        .map_err(|e| {
            format!("Could not read the end of the Brown Dust II package ({e}). {DAMAGED}")
        })?;
    on_progress(written);

    let counting = reader.into_inner();
    let stream_bytes = counting.total;
    let stream_crc = crc_of(counting.hasher);

    if expect_bytes > 0 && stream_bytes != expect_bytes {
        return Err(format!(
            "The Brown Dust II package unpacked to {stream_bytes} bytes but the manifest promised {expect_bytes}. {DAMAGED}"
        ));
    }
    if stream_crc != expect_crc {
        return Err(format!(
            "The Brown Dust II package failed its checksum (expected {expect_crc:08X}, got {stream_crc:08X}). {DAMAGED}"
        ));
    }

    log::info!(
        "Brown Dust II: read {} file(s), {stream_bytes} bytes, checksum {stream_crc:08X} verified.",
        files.len()
    );
    Ok(files)
}

// ------------ Install Preview ------------
// The size of the game package.
pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    let package = fetch_package(profile).await?;
    Ok(package_preview(package.version.clone(), package.download_bytes, package.install_bytes, 1))
}

// ------------ Publisher ------------
// An install fetches the package and unpacks it. A repair and a verify check the files the install record lists by CRC32,
// and a repair fetches the package again for the bad ones. An install with no record, such as the official starter's, can
// only be checked by its shape, and a repair of it unpacks the whole package over it to make the record.
#[derive(Default)]
pub(super) struct Bd2 {
    settings: Mutex<Value>,
}

impl Publisher for Bd2 {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let record = validator::recorded(cx.dir).await;
            if cx.purpose == JobKind::Verify {
                return Ok(Manifest { version: record.version.clone(), entries: record.entries(), ..Default::default() });
            }
            let package = fetch_package(cx.profile).await?;
            *self.settings.lock() = package.settings;
            let part = Entry::new(package.file_name.as_str(), package.download_bytes, Hash::None, package.url.as_str());
            let recorded = record.entries();
            let installed: u64 = recorded.iter().map(|f| f.size).sum();
            let container = Container {
                parts: vec![part],
                unpack: Some(Arc::new(Tied { crc: package.tied_crc, bytes: package.install_bytes })),
                room: package.install_bytes.saturating_sub(installed),
            };
            let entries = if cx.purpose == JobKind::Repair { recorded } else { Vec::new() };
            Ok(Manifest { version: package.version, entries, mirrors: origin_of(&container.parts), containers: vec![container], ..Default::default() })
        })
    }

    fn after_seal<'a>(
        &'a self,
        _job: &'a Arc<JobCore>,
        dir: &'a Path,
        manifest: &'a Manifest,
        sealed: &'a Sealed,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let settings = self.settings.lock().clone();
            write_launcher_settings(dir, &settings)?;
            validator::update_game_config_file(sealed, dir, &manifest.version).map(|()| true)
        })
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({ "version": manifest_version(&fetch_game_manifest(profile).await?)? })) })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn write_space(&self, download: u64, install: u64) -> (u64, f64) {
        (download.saturating_add(install), 1.0)
    }

    fn installed_version(&self, dir: &Path) -> Option<String> {
        local_game_version_at(dir).or_else(|| installed_version(dir))
    }

    fn policy(&self, dir: &Path) -> Policy {
        Policy { checksums: true, records: true, partial_verify: validator::Record::load(dir).is_empty(), ..Policy::default() }
    }

    fn shape_check<'a>(&'a self, cx: &'a Cx<'a>) -> Option<BoxFuture<'a, Shape>> {
        validator::Record::load(cx.dir).is_empty().then(|| -> BoxFuture<'a, Shape> {
            Box::pin(async move {
                let name = game_profiles::display_name(cx.profile);
                let mut problems = Vec::new();
                if !cx.dir.join(SETTINGS_FILE).is_file() {
                    problems.push(format!(
                        "{SETTINGS_FILE} is missing from {}, so {name} cannot tell which service to sign in to. Reinstall it.",
                        cx.dir.display()
                    ));
                }
                let published = match fetch_package(cx.profile).await {
                    Ok(package) => Some((package.version, package.install_bytes)),
                    Err(e) => {
                        log::warn!("{name} verify: could not reach Neowiz for the published size ({e}); skipped the size check.");
                        None
                    }
                };
                let note = ". Peebify did not install this copy, so it has no per-file checksums for it. This checks the shape of the install rather than each file. Update or reinstall through Peebify once and every later check compares every file.";
                Shape { problems, published, note: note.to_string() }
            })
        })
    }
}

struct Tied {
    crc: u32,
    bytes: u64,
}

impl Unpacker for Tied {
    fn is_package(&self, name: &str) -> bool {
        package_version(name).is_some()
    }

    fn unpack<'a>(
        &'a self,
        job: &'a Arc<JobCore>,
        dir: &'a Path,
        archive: &'a Path,
        wanted: Option<&'a HashSet<String>>,
    ) -> BoxFuture<'a, Result<Unpacked, String>> {
        Box::pin(async move {
            let name = game_profiles::display_name(job.profile());
            let report = job.unpacking(archive, self.bytes, wanted.map_or(1, HashSet::len), format!("Unpacking {name}..."));
            let (package, target, wanted, cancel) = (archive.to_path_buf(), dir.to_path_buf(), wanted.cloned(), Arc::clone(job.gate.flag()));
            let (crc, bytes) = (self.crc, self.bytes);
            let files = tauri::async_runtime::spawn_blocking(move || {
                extract_package(&package, &target, wanted.as_ref(), crc, bytes, &cancel, |done| {
                    report(if bytes > 0 { done as f64 / bytes as f64 * 100.0 } else { 0.0 });
                })
            })
            .await
            .map_err(|e| format!("{name} unpack task panicked: {e}"))?
            .inspect_err(|e| {
                if is_damaged_package_error(e) {
                    match std::fs::remove_file(archive) {
                        Ok(()) => log::warn!("{name}: removed the damaged package so the next attempt downloads it again."),
                        Err(err) => log::warn!("{name}: could not remove the damaged package ({err})."),
                    }
                }
            })?;
            Ok(Unpacked { written: Extracted::files(dir, files.iter().map(|f| &*f.path)), files })
        })
    }
}

// ------------ Tests ------------
// Unpacking a package, the install's version and record, the leftover packages and the settings file.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::validator::{FileCheck, FileValidator};
    use std::path::PathBuf;

    #[test]
    fn the_size_fallback_adds_the_zip_trailer_to_the_listed_size() {
        assert_eq!(fallback_download_bytes(373_564_702), 373_564_786);
        assert_eq!(fallback_download_bytes(0), 0);
    }

    fn crc_of(bytes: &[u8]) -> u32 {
        crc32fast::hash(bytes)
    }

    fn broken(dir: &Path, files: &[Entry]) -> Vec<String> {
        files
            .iter()
            .filter(|f| FileValidator::check(&dir.join(&*f.path), f.size, &f.hash.expected(), None, |_| {}) != Ok(FileCheck::Valid))
            .map(|f| f.path.to_string())
            .collect()
    }

    #[derive(Default)]
    struct ZipBuilder {
        body: Vec<u8>,
        central: Vec<u8>,
        entries: u16,
    }

    impl ZipBuilder {
        fn add(&mut self, name: &str, data: &[u8], flags: u16) {
            self.add_raw(name, data, 0, crc_of(data), data.len() as u32, flags);
        }

        fn add_raw(
            &mut self,
            name: &str,
            stored: &[u8],
            method: u16,
            crc: u32,
            uncompressed: u32,
            flags: u16,
        ) {
            let offset = self.body.len() as u32;
            let name = name.as_bytes();
            self.body.extend_from_slice(&LOCAL_FILE_SIG.to_le_bytes());
            self.body.extend_from_slice(&20u16.to_le_bytes());
            self.body.extend_from_slice(&flags.to_le_bytes());
            self.body.extend_from_slice(&method.to_le_bytes());
            self.body.extend_from_slice(&0u32.to_le_bytes());
            self.body.extend_from_slice(&crc.to_le_bytes());
            self.body
                .extend_from_slice(&(stored.len() as u32).to_le_bytes());
            self.body.extend_from_slice(&uncompressed.to_le_bytes());
            self.body
                .extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.body.extend_from_slice(&0u16.to_le_bytes());
            self.body.extend_from_slice(name);
            self.body.extend_from_slice(stored);

            self.central
                .extend_from_slice(&CENTRAL_DIR_SIG.to_le_bytes());
            self.central.extend_from_slice(&20u16.to_le_bytes());
            self.central.extend_from_slice(&20u16.to_le_bytes());
            self.central.extend_from_slice(&flags.to_le_bytes());
            self.central.extend_from_slice(&method.to_le_bytes());
            self.central.extend_from_slice(&0u32.to_le_bytes());
            self.central.extend_from_slice(&crc.to_le_bytes());
            self.central
                .extend_from_slice(&(stored.len() as u32).to_le_bytes());
            self.central.extend_from_slice(&uncompressed.to_le_bytes());
            self.central
                .extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.central.extend_from_slice(&0u16.to_le_bytes());
            self.central.extend_from_slice(&0u16.to_le_bytes());
            self.central.extend_from_slice(&0u16.to_le_bytes());
            self.central.extend_from_slice(&0u16.to_le_bytes());
            self.central.extend_from_slice(&0u32.to_le_bytes());
            self.central.extend_from_slice(&offset.to_le_bytes());
            self.central.extend_from_slice(name);
            self.entries += 1;
        }

        fn finish(mut self) -> Vec<u8> {
            let offset = self.body.len() as u32;
            let size = self.central.len() as u32;
            self.body.append(&mut self.central);
            self.body.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
            self.body.extend_from_slice(&0u16.to_le_bytes());
            self.body.extend_from_slice(&0u16.to_le_bytes());
            self.body.extend_from_slice(&self.entries.to_le_bytes());
            self.body.extend_from_slice(&self.entries.to_le_bytes());
            self.body.extend_from_slice(&size.to_le_bytes());
            self.body.extend_from_slice(&offset.to_le_bytes());
            self.body.extend_from_slice(&0u16.to_le_bytes());
            self.body
        }
    }

    fn inner_zip(entries: &[(&str, &[u8])], flags: u16) -> Vec<u8> {
        let mut zip = ZipBuilder::default();
        for (name, data) in entries {
            zip.add(name, data, flags);
        }
        zip.finish()
    }

    fn wrap_as_package(inner: &[u8], version: &str) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(inner).unwrap();
        let compressed = encoder.finish().unwrap();

        let entry = format!("{version}.tied");
        let entry = entry.as_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(&LOCAL_FILE_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&crc_of(inner).to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        out.extend_from_slice(&(entry.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(entry);
        out.extend_from_slice(&compressed);
        out.extend_from_slice(&CENTRAL_DIR_SIG.to_le_bytes());
        out
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "peebify-bd2-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("BrownDust II.exe", b"MZ fake client bytes".as_slice()),
            ("BrownDust II_Data/", b"".as_slice()),
            ("BrownDust II_Data/level0", b"unity scene payload".as_slice()),
            ("UnityPlayer.dll", b"engine payload".as_slice()),
        ]
    }

    #[test]
    fn unpacks_a_nested_package_and_records_every_file() {
        let inner = inner_zip(&sample(), 0);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("src");
        let out = Scratch::new("out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let files = extract_package(
            &archive,
            out.path(),
            None,
            crc_of(&inner),
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect("a well-formed package unpacks");

        assert_eq!(files.len(), 3, "directories are not catalogued as files");
        assert_eq!(
            std::fs::read(out.path().join("BrownDust II.exe")).unwrap(),
            b"MZ fake client bytes"
        );
        assert!(out.path().join("BrownDust II_Data").is_dir());
        assert_eq!(
            std::fs::read(out.path().join("BrownDust II_Data/level0")).unwrap(),
            b"unity scene payload"
        );

        assert!(broken(out.path(), &files).is_empty(), "freshly unpacked files must verify");
        std::fs::write(out.path().join("UnityPlayer.dll"), b"engine payloaD").unwrap();
        std::fs::remove_file(out.path().join("BrownDust II.exe")).unwrap();
        assert_eq!(broken(out.path(), &files), ["BrownDust II.exe", "UnityPlayer.dll"], "a missing file and one of the same size");
    }

    #[test]
    fn realigns_after_entries_that_leave_compressed_bytes_unread() {
        let payload = b"deflated payload deflated payload deflated payload".as_slice();
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(payload).unwrap();
        let mut deflated = encoder.finish().unwrap();
        deflated.extend_from_slice(b"PAD!");
        let mut padded = b"stored".to_vec();
        padded.extend_from_slice(b"PAD!");

        let mut zip = ZipBuilder::default();
        zip.add_raw("dir/", b"junk", 0, 0, 0, 0);
        zip.add_raw("dir/a.bin", &deflated, 8, crc_of(payload), payload.len() as u32, 0);
        zip.add_raw("dir/b.bin", &padded, 0, crc_of(b"stored"), 6, 0);
        zip.add("dir/c.bin", b"tail", 0);
        let inner = zip.finish();
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("realign-src");
        let out = Scratch::new("realign-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let files = extract_package(
            &archive,
            out.path(),
            None,
            crc_of(&inner),
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect("leftover entry bytes are skipped, not read as the next header");

        assert_eq!(files.len(), 3);
        assert_eq!(std::fs::read(out.path().join("dir/a.bin")).unwrap(), payload);
        assert_eq!(std::fs::read(out.path().join("dir/b.bin")).unwrap(), b"stored");
        assert_eq!(std::fs::read(out.path().join("dir/c.bin")).unwrap(), b"tail");
    }

    #[test]
    fn a_truncated_inner_header_is_marked_damaged() {
        let mut inner = inner_zip(&sample(), 0);
        inner.truncate(10);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("trunc-src");
        let out = Scratch::new("trunc-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let err = extract_package(
            &archive,
            out.path(),
            None,
            crc_of(&inner),
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect_err("a stream that ends inside a file header must fail");
        assert!(err.contains("package file header"), "{err}");
        assert!(is_damaged_package_error(&err), "{err}");
    }

    #[test]
    fn writes_only_the_wanted_entries_but_still_catalogues_all() {
        let inner = inner_zip(&sample(), 0);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("subset-src");
        let out = Scratch::new("subset-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let wanted: HashSet<String> = HashSet::from(["UnityPlayer.dll".to_string()]);
        let cancel = AtomicBool::new(false);
        let files = extract_package(
            &archive,
            out.path(),
            Some(&wanted),
            crc_of(&inner),
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect("a subset unpack succeeds");

        assert_eq!(files.len(), 3, "the manifest still covers every file");
        assert!(out.path().join("UnityPlayer.dll").is_file());
        assert!(!out.path().join("BrownDust II.exe").exists());
    }

    #[test]
    fn rejects_a_package_whose_checksum_does_not_match() {
        let inner = inner_zip(&sample(), 0);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("crc-src");
        let out = Scratch::new("crc-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let err = extract_package(
            &archive,
            out.path(),
            None,
            0xDEAD_BEEF,
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect_err("a wrong tied_crc must fail");
        assert!(err.contains("failed its checksum"), "{err}");
        assert!(is_damaged_package_error(&err), "{err}");
    }

    #[test]
    fn rejects_a_package_whose_unpacked_size_does_not_match() {
        let inner = inner_zip(&sample(), 0);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("size-src");
        let out = Scratch::new("size-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let err = extract_package(
            &archive,
            out.path(),
            None,
            crc_of(&inner),
            inner.len() as u64 + 1,
            &cancel,
            |_| {},
        )
        .expect_err("a wrong unziped_size must fail");
        assert!(err.contains("the manifest promised"), "{err}");
    }

    #[test]
    fn refuses_an_entry_that_escapes_the_install_folder() {
        let inner = inner_zip(&[("../escaped.txt", b"nope".as_slice())], 0);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("slip-src");
        let out = Scratch::new("slip-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let err = extract_package(
            &archive,
            out.path(),
            None,
            crc_of(&inner),
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect_err("a path escaping the install folder must be refused");
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn refuses_an_entry_that_uses_a_trailing_data_descriptor() {
        let inner = inner_zip(&[("thing.bin", b"payload".as_slice())], 0x0008);
        let package = wrap_as_package(&inner, "20260905000");
        let src = Scratch::new("dd-src");
        let out = Scratch::new("dd-out");
        let archive = src.write("20260905000.tied.dat", &package);

        let cancel = AtomicBool::new(false);
        let err = extract_package(
            &archive,
            out.path(),
            None,
            crc_of(&inner),
            inner.len() as u64,
            &cancel,
            |_| {},
        )
        .expect_err("a streamed entry must be refused rather than mis-parsed");
        assert!(err.contains("data descriptor"), "{err}");
    }

    #[test]
    fn an_older_peebify_record_gives_the_version_and_the_checked_files() {
        let out = Scratch::new("record");
        out.write(
            "peebify_bd2_files.json",
            br#"{"version":"20260905000","files":[{"path":"a.bin","size":7,"crc":"F0A0E438"},{"path":"sub/b.bin","size":9,"crc":"00000001"}]}"#,
        );

        assert_eq!(installed_version(out.path()).as_deref(), Some("20260905000"));
        let files = validator::Record::load(out.path()).entries();
        assert_eq!(files.len(), 2);
        assert_eq!((&*files[0].path, files[0].size, files[0].hash), ("a.bin", 7, Hash::Crc32(0xF0A0_E438)));
        assert_eq!(files[1].hash, Hash::Crc32(1));
    }

    #[test]
    fn adopts_the_version_from_a_package_left_by_the_official_starter() {
        let out = Scratch::new("adopt");
        std::fs::write(out.path().join("20260827002.tied.dat"), b"old").unwrap();
        std::fs::write(out.path().join("20260905000.tied.dat"), b"new").unwrap();
        std::fs::write(out.path().join("notes.tied.dat"), b"not a version").unwrap();

        assert_eq!(
            installed_version(out.path()).as_deref(),
            Some("20260905000")
        );
    }

    #[test]
    fn a_package_left_by_a_failed_peebify_update_does_not_count_as_installed() {
        let out = Scratch::new("adopt_marker");
        out.write("20260905000.tied.dat", b"new");
        out.write(crate::backend::validator::INSTALL_MARKER_FILE, b"{}");

        assert_eq!(installed_version(out.path()), None);
    }

    struct Named;

    impl Unpacker for Named {
        fn is_package(&self, name: &str) -> bool {
            Tied { crc: 0, bytes: 0 }.is_package(name)
        }

        fn unpack<'a>(&'a self, _: &'a Arc<JobCore>, _: &'a Path, _: &'a Path, _: Option<&'a HashSet<String>>) -> BoxFuture<'a, Result<Unpacked, String>> {
            Box::pin(async { Err(String::new()) })
        }
    }

    #[test]
    fn packages_of_other_builds_go_and_nothing_else() {
        let out = Scratch::new("stale");
        let old = out.write("20260827002.tied.dat", b"old");
        let old_part = out.write("20260827002.tied.dat.part", b"partial");
        let current = out.write("20260905000.tied.dat", b"cur");
        let notes = out.write("notes.tied.dat", b"keep");
        let container = Container {
            parts: vec![Entry::new("20260905000.tied.dat", 3, Hash::None, "")],
            unpack: Some(Arc::new(Named)),
            room: 0,
        };

        crate::backend::extract::remove_other_packages(out.path(), &container);
        assert!(!old.exists() && !old_part.exists());
        assert!(current.exists() && notes.exists());
    }

    #[test]
    fn only_a_plain_package_file_name_is_taken_from_the_manifest() {
        assert_eq!(
            package_file_name("/20260905000.tied.dat").as_deref(),
            Ok("20260905000.tied.dat")
        );
        assert_eq!(
            package_file_name("files/Game_v2-1.tied.dat").as_deref(),
            Ok("Game_v2-1.tied.dat")
        );
        for bad in [
            "",
            "/",
            "files/",
            ".",
            "..",
            "files/..",
            r"..\..\x.dat",
            "C:x.dat",
            "x.dat.",
            ".hidden",
            "a b.dat",
        ] {
            assert!(package_file_name(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn launcher_settings_are_written_from_the_manifest_and_required() {
        let out = Scratch::new("settings");
        write_launcher_settings(out.path(), &json!({ "service": "bd2", "ver": 1 })).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(out.path().join(SETTINGS_FILE)).unwrap())
                .unwrap();
        assert_eq!(written["service"], "bd2");
        assert_eq!(written["ver"], 1);
        assert!(!out.path().join(format!("{SETTINGS_FILE}.tmp")).exists());

        let err = write_launcher_settings(out.path(), &Value::Null).unwrap_err();
        assert!(err.contains(SETTINGS_FILE), "{err}");
    }

    #[test]
    fn merges_an_extends_chain_with_the_child_winning() {
        let mut merged = json!({});
        for doc in [
            json!({
                "download_url": "https://example.test/base",
                "exe_file": "BrownDust II.exe",
                "install_path": "Browndust2_10000001",
                "settings": { "service": "bd2_gpg", "ver": 1 },
                "last_file": { "file": "/1.tied.dat", "tied_crc": "AABBCCDD" }
            }),
            json!({
                "install_path": "BrownDust2_10000002",
                "settings": { "service": "bd2" }
            }),
        ] {
            merge_into(&mut merged, doc);
        }

        assert_eq!(merged["install_path"], "BrownDust2_10000002");
        assert_eq!(merged["settings"]["service"], "bd2");
        assert_eq!(merged["settings"]["ver"], 1, "untouched parent keys survive");
        assert_eq!(merged["exe_file"], "BrownDust II.exe");
        assert_eq!(merged["last_file"]["tied_crc"], "AABBCCDD");
    }

    #[test]
    fn manifest_version_prefers_last_file_and_ignores_package_details() {
        let full = json!({
            "last_version": "20260101000",
            "last_file": { "version": "20260905000", "tied_crc": "not hex" }
        });
        assert_eq!(manifest_version(&full).unwrap(), "20260905000");
        let fallback = json!({ "last_version": "20260101000", "last_file": {} });
        assert_eq!(manifest_version(&fallback).unwrap(), "20260101000");
        assert!(manifest_version(&json!({ "last_file": { "version": "" } })).is_err());
    }

    fn real_fixture() -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/bd2-fixture/20260905000.tied.dat");
        path.is_file().then_some(path)
    }

    #[test]
    fn unpacks_the_published_package_when_the_fixture_is_present() {
        let Some(package) = real_fixture() else {
            eprintln!("target/bd2-fixture is empty; skipping the published-package check");
            return;
        };
        let out = Scratch::new("real");
        let cancel = AtomicBool::new(false);
        let mut ticks = 0usize;
        let files = extract_package(
            &package,
            out.path(),
            None,
            0xF0A0_E438,
            892_948_987,
            &cancel,
            |_| ticks += 1,
        )
        .expect("the published package unpacks");

        assert_eq!(files.len(), 2207);
        assert!(ticks > 50, "a long unpack reports progress");
        assert_eq!(
            std::fs::metadata(out.path().join("BrownDust II.exe"))
                .unwrap()
                .len(),
            677_512
        );

        let broken = broken(out.path(), &files);
        assert!(broken.is_empty(), "{} files failed", broken.len());
    }
}
