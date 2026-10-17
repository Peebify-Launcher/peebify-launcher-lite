// ------------ Archive Extraction ------------
// Runs the unpacking tools games ship in (7-Zip and hpatchz) below normal priority while a game runs, flushes what
// they wrote before anything records it as installed, and weighs each archive's share of the unpacking progress.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use peebify_transfer::part::discard_part;
use peebify_transfer::Sealed;
use serde_json::json;
use tauri::{AppHandle, Manager};

use super::engine::{GameDownloadManager, JobCore};
use super::fs_util::safe_join;
use super::manifest::{Container, Entry};
use super::queue::Phase;
use super::validator::PART_ARTIFACT_SUFFIXES;

// ------------ 7-Zip ------------
// Finds the extractor (the bundled copy, or one on the system) and runs it with progress.
pub(super) fn is_damaged_archive_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "data error",
        "crc failed",
        "headers error",
        "unexpected end of archive",
        "can not open the file as archive",
        "is not archive",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn tool_creation_flags() -> u32 {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    if super::http::shared_transfer().is_playing() {
        CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
    } else {
        CREATE_NO_WINDOW
    }
}

pub async fn run_7z_extract(
    seven_zip: &Path,
    archive: &Path,
    install_path: &Path,
    mut on_percent: impl FnMut(f64),
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let mut cmd = tokio::process::Command::new(seven_zip);
    cmd.arg("x")
        .arg(archive)
        .arg("-y")
        .arg("-bsp1")
        .arg("-sccUTF-8")
        .arg(format!("-o{}", install_path.display()))
        .current_dir(install_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd.creation_flags(tool_creation_flags());

    let mut child = cmd.spawn().map_err(|e| format!("7z spawn failed: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("failed to capture 7z stdout")?;
    let mut stderr_pipe = child.stderr.take().ok_or("failed to capture 7z stderr")?;

    let stderr_task = tauri::async_runtime::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let percent_re = regex::Regex::new(r"(\d{1,3})%").expect("static regex");
    let mut tail = String::new();
    let mut buf = vec![0u8; 4096];
    loop {
        let n = stdout
            .read(&mut buf)
            .await
            .map_err(|e| format!("7z stdout read failed: {e}"))?;
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..n]);
        let combined = format!("{tail}{text}");
        if let Some(m) = percent_re
            .captures_iter(&combined)
            .last()
            .and_then(|c| c[1].parse::<f64>().ok())
        {
            on_percent(m);
        }
        tail = combined
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("7z wait failed: {e}"))?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(format!(
            "Process exited with code {code} ({}) while extracting {}: {}",
            seven_zip_exit_meaning(code),
            archive.display(),
            if stderr_text.trim().is_empty() {
                "unknown error"
            } else {
                stderr_text.trim()
            }
        ));
    }
    Ok(())
}

fn seven_zip_exit_meaning(code: i32) -> &'static str {
    match code {
        1 => "warning",
        2 => "fatal error",
        7 => "command line error",
        8 => "not enough memory",
        255 => "stopped by user",
        _ => "unexpected exit",
    }
}

pub async fn resolve_7z_binary(app: &AppHandle) -> Result<PathBuf, String> {
    resolve_7z(app, true).await
}

pub async fn resolve_bundled_7z_binary(app: &AppHandle) -> Result<PathBuf, String> {
    resolve_7z(app, false).await
}

async fn resolve_7z(app: &AppHandle, allow_system: bool) -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Ok(env_path) = std::env::var("PEEBIFY_7Z_PATH") {
        if !env_path.is_empty() {
            return Ok(PathBuf::from(env_path));
        }
    }
    if let Ok(resource_dir) = app.path().resource_dir() {
        for candidate in [
            resource_dir.join("resources").join("7z.exe"),
            resource_dir.join("7z.exe"),
        ] {
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    if cfg!(debug_assertions) {
        let dev_resources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
        for candidate in [dev_resources.join("7z.exe")] {
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    if !allow_system {
        return Err(
            "The extractor bundled with Peebify is missing. Repair the launcher by reinstalling it, then try again."
                .to_string(),
        );
    }
    let mut cmd = tokio::process::Command::new("where");
    cmd.arg("7z")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.creation_flags(0x0800_0000);
    if let Ok(status) = cmd.status().await {
        if status.success() {
            log::warn!("Peebify's bundled extractor is missing, so 7-Zip from PATH is used.");
            return Ok(PathBuf::from("7z"));
        }
    }
    Err("No 7z extractor found. Reinstall Peebify to restore its bundled extractor, or install 7-Zip.".to_string())
}

pub(super) fn downloaded_package_paths(install_path: &Path, parts: &[Entry]) -> Vec<PathBuf> {
    parts
        .iter()
        .filter_map(|p| Path::new(&*p.path).file_name())
        .map(|name| install_path.join(name))
        .collect()
}

pub(super) fn remove_downloaded_packages(packages: &[PathBuf]) {
    if packages.is_empty() {
        return;
    }
    for fp in packages {
        if let Err(e) = std::fs::remove_file(fp) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Failed to remove downloaded package {}: {e}", fp.display());
            }
        }
    }
    log::info!(
        "Package cleanup finished for {} manifest entr{}.",
        packages.len(),
        if packages.len() == 1 { "y" } else { "ies" }
    );
}

// ------------ Split Archives ------------
// Endfield and Reverse: 1999 ship their game as archives (whole, or split into .001 parts) that are unpacked once
// they have all landed, with progress weighted by each archive's size.
pub(super) fn needs_unpack(install_path: &Path, version: &str) -> bool {
    match super::game_manager::local_game_version_at(install_path) {
        Some(local) if super::fs_util::same_version(&local, version) => {
            log::info!(
                "Split-archive install: installed version {local} matches remote {version}; skipping package validation (installer archives may already be removed after extraction)."
            );
            false
        }
        _ => true,
    }
}

pub(super) async fn split_archives(
    job: &GameDownloadManager,
    install_path: &Path,
    parts: &[Entry],
    sealed: Sealed,
) -> Result<(Vec<PathBuf>, Sealed), String> {
    let dest_set: HashSet<String> = parts
        .iter()
        .map(|p| p.path.to_string())
        .filter(|d| !d.is_empty())
        .collect();
    let disk_files: Vec<String> = std::fs::read_dir(install_path)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();

    let seven_zip = resolve_7z_binary(&job.app).await?;
    log::info!("Using extractor binary: {}", seven_zip.display());

    let is_first_part = |name: &str| {
        let lower = name.to_lowercase();
        lower.ends_with(".zip.001") || lower.ends_with(".7z.001")
    };

    let split_first_parts: Vec<PathBuf> = disk_files
        .iter()
        .filter(|f| is_first_part(f) && (dest_set.is_empty() || dest_set.contains(*f)))
        .map(|f| install_path.join(f))
        .collect();

    let archive_ext = |name: &str| {
        let lower = name.to_lowercase();
        lower.ends_with(".zip") || lower.ends_with(".7z")
    };
    let mut standalone: Vec<PathBuf> = Vec::new();
    for dest in &dest_set {
        if !archive_ext(dest) {
            continue;
        }
        let stem = dest.rsplit_once('.').map(|(s, _)| s).unwrap_or(dest);
        if dest_set.contains(&format!("{stem}.zip.001"))
            || dest_set.contains(&format!("{stem}.7z.001"))
        {
            continue;
        }
        let Ok(fp) = crate::backend::fs_util::safe_join(install_path, dest) else {
            continue;
        };
        if fp.exists() {
            standalone.push(fp);
        }
    }

    let mut archives = split_first_parts;
    archives.extend(standalone);

    if archives.is_empty() {
        log::warn!(
            "No archives (.zip / .zip.001 / .7z / .7z.001) found to extract for {}.",
            job.profile_id()
        );
        return Ok((Vec::new(), sealed));
    }

    let _extracting = job.begin_extracting();
    job.note_install_write();
    job.send_progress(
        Phase::Extracting,
        "Extracting packages...",
        json!({ "message": "Downloaded packages complete. Extracting game files..." }),
    );

    let total_archive_bytes: f64 = parts.iter().map(|p| p.size as f64).sum();
    let extraction = Arc::new(Mutex::new(ExtractionProgress::new(
        archives.len(),
        archive_weights(&archives, parts),
    )));

    let mut written = Vec::new();
    for archive in &archives {
        let base_name = archive
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        log::info!("Extracting package: {base_name}");
        let prior_percent = extraction.lock().percent_of(archive);
        job.emit_extraction_progress(
            &extraction,
            archive,
            prior_percent,
            total_archive_bytes,
            &format!("Extracting {base_name}..."),
        );

        job.gate
            .cancellable(run_7z_extract(&seven_zip, archive, install_path, |percent| {
                job.emit_extraction_progress(
                    &extraction,
                    archive,
                    percent,
                    total_archive_bytes,
                    &format!("Extracting {base_name}..."),
                );
            }))
            .await
            .map_err(|e| job.cancel_aware(e))
            .inspect_err(|e| {
                if !job.is_cancelled() && is_damaged_archive_error(e) {
                    let _ = std::fs::remove_file(archive);
                    discard_part(archive);
                    log::warn!("Removed the damaged {base_name} so the next attempt downloads it again.");
                }
            })?;
        if job.is_cancelled() {
            return Err("Download aborted by user.".to_string());
        }
        written.push(unpacked_by_7z(&seven_zip, archive, install_path).await);

        job.emit_extraction_progress(
            &extraction,
            archive,
            100.0,
            total_archive_bytes,
            &format!("Extracted {base_name}"),
        );
    }

    let sealed = seal_extracted(written).await?;
    Ok((downloaded_package_paths(install_path, parts), sealed))
}

// ------------ Packages ------------
// A game that ships one package unpacks it with its own reader or tool, which its container carries.
pub(super) const VERIFIED_SUFFIX: &str = ".verified";

pub(super) async fn unpack_package(
    job: &Arc<JobCore>,
    dir: &Path,
    container: &Container,
    wanted: Option<&HashSet<String>>,
) -> Result<(Vec<Entry>, Sealed), String> {
    let (Some(unpacker), Some(part)) = (&container.unpack, container.parts.first()) else {
        return Err("There is no package to unpack.".to_string());
    };
    let unpacked = unpacker.unpack(job, dir, &safe_join(dir, &part.path)?, wanted).await?;
    Ok((unpacked.files, seal_extracted(vec![unpacked.written]).await?))
}

pub(super) fn remove_other_packages(dir: &Path, container: &Container) {
    let (Some(unpacker), Some(part)) = (&container.unpack, container.parts.first()) else {
        return;
    };
    let Ok(keep) = safe_join(dir, &part.path) else {
        return;
    };
    let (Some(folder), Some(keep_name)) = (keep.parent(), keep.file_name().map(|n| n.to_string_lossy().into_owned())) else {
        return;
    };
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = name
            .strip_suffix(VERIFIED_SUFFIX)
            .or_else(|| PART_ARTIFACT_SUFFIXES.iter().find_map(|suffix| name.strip_suffix(suffix)))
            .unwrap_or(&name);
        if stem != keep_name && unpacker.is_package(stem) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => log::info!("Removed {name}, left by an older download."),
                Err(e) => log::warn!("Could not remove the old download {name} ({e})."),
            }
        }
    }
}

// ------------ Flushing Unpacked Files ------------
// What an extractor wrote is flushed to disk before any install record trusts it.
pub enum Extracted {
    Files(Vec<PathBuf>),
    Tree { root: PathBuf, skip: Vec<PathBuf> },
}

impl Extracted {
    pub fn files<'a>(root: &Path, names: impl IntoIterator<Item = &'a str>) -> Self {
        Self::Files(
            names
                .into_iter()
                .filter_map(|name| super::fs_util::safe_join(root, name).ok())
                .collect(),
        )
    }
}

pub async fn seal_extracted(written: Vec<Extracted>) -> Result<Sealed, String> {
    let ledger = peebify_transfer::Ledger::new(None);
    let adopting = Arc::clone(&ledger);
    let count = tauri::async_runtime::spawn_blocking(move || {
        let mut count = 0;
        for set in written {
            match set {
                Extracted::Files(files) => {
                    for file in files.into_iter().filter(|f| f.is_file()) {
                        adopting.adopt_file(file);
                        count += 1;
                    }
                }
                Extracted::Tree { root, skip } => {
                    let skip: Vec<&Path> = skip.iter().map(PathBuf::as_path).collect();
                    count += adopting.adopt_tree(&root, &skip);
                }
            }
        }
        count
    })
    .await
    .map_err(|e| format!("flush task panicked: {e}"))?;
    let started = std::time::Instant::now();
    let sealed = ledger.seal().await?;
    log::info!(
        "Flushed {count} unpacked file(s) to disk in {:.1}s.",
        started.elapsed().as_secs_f64()
    );
    Ok(sealed)
}

pub async fn unpacked_by_7z(seven_zip: &Path, archive: &Path, into: &Path) -> Extracted {
    let mut cmd = tokio::process::Command::new(seven_zip);
    cmd.arg("l")
        .arg("-slt")
        .arg("-ba")
        .arg("-sccUTF-8")
        .arg(archive)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd.creation_flags(tool_creation_flags());
    match cmd.output().await {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            Extracted::files(into, parse_7z_listing(&text).iter().map(String::as_str))
        }
        outcome => {
            let why = match outcome {
                Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
                Err(e) => e.to_string(),
            };
            log::warn!(
                "Could not list {} ({why}), so all of {} is flushed.",
                archive.display(),
                into.display()
            );
            Extracted::Tree {
                root: into.to_path_buf(),
                skip: Vec::new(),
            }
        }
    }
}

fn parse_7z_listing(text: &str) -> Vec<String> {
    let mut files = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let mut path = None;
        let mut folder = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("Path = ") {
                path = Some(p);
            } else if line == "Folder = +" {
                folder = true;
            } else if let Some(attributes) = line.strip_prefix("Attributes = ") {
                folder |= attributes.starts_with('D');
            }
        }
        if let Some(p) = path.filter(|p| !p.is_empty() && !folder) {
            files.push(p.to_string());
        }
    }
    files
}

// ------------ hpatchz ------------
// Duet Night Abyss ships its builds as HDiffPatch archives.
pub async fn run_hpatchz(
    hpatchz: &Path,
    old_dir: Option<&Path>,
    archive: &Path,
    install_path: &Path,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(hpatchz);
    cmd.arg("-f");
    match old_dir {
        Some(old) => cmd.arg(old),
        None => cmd.arg(""),
    };
    cmd.arg(archive)
        .arg(install_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd.creation_flags(tool_creation_flags());

    let output = cmd
        .spawn()
        .map_err(|e| format!("hpatchz spawn failed: {e}"))?
        .wait_with_output()
        .await
        .map_err(|e| format!("hpatchz wait failed: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let detail: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = detail[detail.len().saturating_sub(4)..].join(" | ");
    Err(format!(
        "hpatchz exited with code {} while installing from {}: {}",
        output.status.code().unwrap_or(-1),
        archive.display(),
        if tail.is_empty() { "no output" } else { &tail }
    ))
}

// ------------ Unpacking Progress ------------
// Overall progress across several archives, weighted by their size, with a smoothed speed and time left.
pub(super) struct ExtractionProgress {
    per_archive: HashMap<PathBuf, f64>,
    weights: HashMap<PathBuf, f64>,
    archive_count: usize,
    last_tick_bytes: f64,
    last_tick_time: std::time::Instant,
    smoothed_speed: f64,
}

pub(super) struct ExtractionTick {
    pub(super) percent: f64,
    pub(super) speed: f64,
    pub(super) unpacked: f64,
    pub(super) eta: f64,
}

impl ExtractionProgress {
    pub(super) fn new(archive_count: usize, weights: HashMap<PathBuf, f64>) -> Self {
        Self {
            per_archive: HashMap::new(),
            weights,
            archive_count: archive_count.max(1),
            last_tick_bytes: 0.0,
            last_tick_time: std::time::Instant::now(),
            smoothed_speed: 0.0,
        }
    }

    fn overall_percent(&self) -> f64 {
        let weighted = self.weights.len() == self.archive_count
            && self.weights.values().all(|w| *w > 0.0);
        if !weighted {
            let sum: f64 = self.per_archive.values().sum();
            return (sum / self.archive_count as f64).min(100.0);
        }
        let total: f64 = self.weights.values().sum();
        let done: f64 = self
            .per_archive
            .iter()
            .map(|(archive, pct)| self.weights.get(archive).copied().unwrap_or(0.0) * pct)
            .sum();
        (done / total).min(100.0)
    }

    pub(super) fn percent_of(&self, archive: &Path) -> f64 {
        self.per_archive.get(archive).copied().unwrap_or(0.0)
    }

    pub(super) fn tick(&mut self, archive: &Path, archive_percent: f64, total_bytes: f64) -> ExtractionTick {
        self.per_archive
            .insert(archive.to_path_buf(), archive_percent.clamp(0.0, 100.0));
        let percent = self.overall_percent();
        let unpacked = total_bytes * (percent / 100.0);
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_tick_time).as_secs_f64();
        if dt >= 0.25 {
            let instant = ((unpacked - self.last_tick_bytes) / dt).max(0.0);
            self.smoothed_speed = if self.smoothed_speed > 0.0 {
                self.smoothed_speed * 0.6 + instant * 0.4
            } else {
                instant
            };
            self.last_tick_bytes = unpacked;
            self.last_tick_time = now;
        }
        let remaining = (total_bytes - unpacked).max(0.0);
        let eta = if self.smoothed_speed > 0.0 {
            remaining / self.smoothed_speed
        } else {
            0.0
        };
        ExtractionTick {
            percent,
            speed: self.smoothed_speed,
            unpacked,
            eta,
        }
    }
}

pub(super) fn archive_weights(archives: &[PathBuf], parts: &[Entry]) -> HashMap<PathBuf, f64> {
    let sizes: Vec<(String, u64)> = parts
        .iter()
        .filter_map(|p| {
            let name = Path::new(&*p.path).file_name()?.to_string_lossy().to_lowercase();
            Some((name, p.size))
        })
        .collect();
    archives
        .iter()
        .map(|archive| {
            let name = archive
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let weight: u64 = match name.strip_suffix(".001") {
                Some(stem) => sizes
                    .iter()
                    .filter(|(part, _)| {
                        part.strip_prefix(stem)
                            .and_then(|rest| rest.strip_prefix('.'))
                            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    })
                    .map(|(_, size)| size)
                    .sum(),
                None => sizes
                    .iter()
                    .filter(|(part, _)| *part == name)
                    .map(|(_, size)| size)
                    .sum(),
            };
            (archive.clone(), weight as f64)
        })
        .collect()
}

// ------------ Tests ------------
// The 7-Zip listing parser, damaged archive detection and the size weighting of unpacking progress.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;

    fn res(dest: &str, size: u64) -> Entry {
        Entry::new(dest, size, crate::backend::manifest::Hash::None, "")
    }

    #[test]
    fn a_7z_listing_names_files_and_leaves_out_folders() {
        let listing = "Path = a.txt\r\nFolder = -\r\nSize = 3\r\nAttributes = A\r\n\r\nPath = Data\r\nFolder = +\r\nAttributes = D\r\n\r\nPath = empty\r\nSize = 0\r\nAttributes = D\r\n\r\nPath = Data\\sub\\b é.pak\r\nSize = 3\r\nAttributes = A\r\n\r\n";
        assert_eq!(parse_7z_listing(listing), vec!["a.txt", "Data\\sub\\b é.pak"]);
    }

    #[test]
    fn damaged_archives_are_told_apart_from_other_unpack_failures() {
        assert!(is_damaged_archive_error(
            "Process exited with code 2 (fatal error) while extracting x.zip: ERROR: Data Error : Client/a.pak"
        ));
        assert!(is_damaged_archive_error("ERROR: x.zip Can not open the file as archive"));
        assert!(is_damaged_archive_error("Unexpected end of archive"));
        assert!(!is_damaged_archive_error(
            "Process exited with code 2 (fatal error) while extracting x.zip: There is not enough space on the disk."
        ));
        assert!(!is_damaged_archive_error("cancelled"));
    }

    #[test]
    fn a_big_archive_weighs_more_than_a_voice_pack() {
        let dir = PathBuf::from("C:/Games/Endfield");
        let game = dir.join("game.zip.001");
        let voice = dir.join("voice_en.zip");
        let resources = [
            res("game.zip.001", 20),
            res("game.zip.002", 20),
            res("voice_en.zip", 5),
            res("unrelated.zip.001x", 99),
        ];
        let weights = archive_weights(&[game.clone(), voice.clone()], &resources);
        assert_eq!(weights[&game], 40.0);
        assert_eq!(weights[&voice], 5.0);

        let mut ex = ExtractionProgress::new(2, weights);
        ex.per_archive.insert(game.clone(), 50.0);
        let half_of_game = ex.overall_percent();
        assert!((half_of_game - 20.0 / 45.0 * 100.0).abs() < 1e-9, "{half_of_game}");
        ex.per_archive.insert(game, 100.0);
        ex.per_archive.insert(voice, 100.0);
        assert!((ex.overall_percent() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn extraction_falls_back_to_equal_weights_when_a_size_is_unknown() {
        let a = PathBuf::from("a.zip");
        let b = PathBuf::from("b.zip");
        let weights = archive_weights(&[a.clone(), b.clone()], &[res("a.zip", 10)]);
        assert_eq!(weights[&b], 0.0);
        let mut ex = ExtractionProgress::new(2, weights);
        ex.per_archive.insert(a, 100.0);
        assert_eq!(ex.overall_percent(), 50.0);

        let mut single = ExtractionProgress::new(1, HashMap::new());
        single.per_archive.insert(PathBuf::from("pkg.zip"), 30.0);
        assert_eq!(single.overall_percent(), 30.0);
    }

    #[test]
    fn downloaded_packages_are_listed_and_removed_from_the_install_root() {
        let dir = TempDir::new("packages");
        let first = dir.write("game.zip.001", b"a");
        let second = dir.write("game.zip.002", b"b");
        let unpacked = dir.write("Game/data.bin", b"c");
        let packages = downloaded_package_paths(
            dir.path(),
            &[
                res("game.zip.001", 1),
                res("sub/game.zip.002", 1),
                res("", 0),
                res("gone.zip", 1),
            ],
        );
        assert_eq!(
            packages,
            vec![first.clone(), second.clone(), dir.path().join("gone.zip")]
        );
        remove_downloaded_packages(&packages);
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(unpacked.exists());
    }
}
