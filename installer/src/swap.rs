// ------------ Install File Swap ------------
// The part of install that replaces files. Old copies of ours move to .backup, the unpacked ones in .staged move in,
// and a failure rolls everything back. The games folder and files that are not ours are never touched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::consts;
use crate::ilog::ilog;
use crate::install::InstallManifest;
use crate::paths;
use crate::win;

fn is_preserved(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy();
    n == consts::STAGED_DIR_NAME
        || n == consts::BACKUP_DIR_NAME
        || n == consts::UPDATE_STATE_NAME
        || n.eq_ignore_ascii_case(consts::GAMES_DIR_NAME)
}

fn top_name(rel: &str) -> &str {
    rel.split(['/', '\\']).find(|s| !s.is_empty()).unwrap_or("")
}

pub fn expected_top_names() -> Vec<String> {
    [consts::MAIN_BINARY, "resources", "icons"]
        .iter()
        .map(|n| n.to_string())
        .collect()
}

pub fn foreign_collisions(dir: &Path, staged_files: &[String]) -> Vec<String> {
    let ours_already = [
        consts::INSTALL_MANIFEST_NAME,
        consts::MAIN_BINARY,
        consts::UNINSTALLER_NAME,
    ]
    .iter()
    .any(|name| dir.join(name).exists());
    if ours_already || paths::is_default_install_dir(dir) {
        return Vec::new();
    }
    let tops: Vec<String> = staged_files
        .iter()
        .map(|rel| top_name(rel).to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut hits: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| !is_preserved(name))
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| tops.contains(&name.to_lowercase()))
        .collect();
    hits.sort();
    hits
}

pub fn collision_message(names: &[String]) -> String {
    format!(
        "This folder already has {} named {}, which setup would replace. Choose an empty folder, or add \"{}\" to the end of the path.",
        if names.len() == 1 { "an item" } else { "items" },
        names.join(" and "),
        consts::PRODUCT_NAME
    )
}

pub fn replaceable_names(old: Option<&InstallManifest>, staged_files: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: String| {
        let name = name.to_lowercase();
        let preserved = is_preserved(std::ffi::OsStr::new(&name));
        if !name.is_empty() && !preserved && !names.contains(&name) {
            names.push(name);
        }
    };

    let top = |rel: &str| top_name(rel).to_string();
    for rel in old.iter().flat_map(|m| m.files.iter()) {
        push(top(rel));
    }
    for rel in staged_files {
        push(top(rel));
    }
    push(consts::UNINSTALLER_NAME.to_string());
    push(consts::INSTALL_MANIFEST_NAME.to_string());
    names
}

pub fn swap(
    dir: &Path,
    staged: &Path,
    backup: &Path,
    replaceable: &[String],
    installed: &mut Vec<PathBuf>,
) -> Result<(), String> {
    std::fs::create_dir_all(backup).map_err(|e| format!("create backup dir: {e}"))?;

    for entry in std::fs::read_dir(dir).map_err(|e| format!("read install dir: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if is_preserved(&name) {
            continue;
        }
        if !replaceable.contains(&name.to_string_lossy().to_lowercase()) {
            ilog!("swap: leaving {} alone (not ours)", name.to_string_lossy());
            continue;
        }
        let from = entry.path();
        let to = backup.join(&name);
        win::retry(20, Duration::from_millis(250), || {
            std::fs::rename(&from, &to)
        })
        .map_err(|e| format!("backup {}: {e}", from.display()))?;
    }

    for entry in std::fs::read_dir(staged).map_err(|e| format!("read staged dir: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dir.join(entry.file_name());
        win::retry(20, Duration::from_millis(250), || {
            std::fs::rename(&from, &to)
        })
        .map_err(|e| format!("install {}: {e}", to.display()))?;
        installed.push(to);
    }
    Ok(())
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub fn roll_back_swap(dir: &Path, backup: &Path, installed: &[PathBuf]) -> Result<(), String> {
    let mut failures: Vec<String> = Vec::new();
    for path in installed {
        let Some(name) = path.file_name() else {
            continue;
        };
        if backup.join(name).exists() {
            continue;
        }
        if let Err(e) = win::retry(20, Duration::from_millis(250), || remove_entry(path)) {
            ilog!("rollback: could not remove {} ({e})", path.display());
            failures.push(format!("remove {}: {e}", path.display()));
        }
    }
    if backup.exists() {
        if let Err(e) = restore_backup(dir, backup) {
            failures.push(e);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

pub fn restore_backup(dir: &Path, backup: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(backup).map_err(|e| format!("read backup dir: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dir.join(entry.file_name());
        let _ = remove_entry(&to);
        win::retry(20, Duration::from_millis(250), || {
            std::fs::rename(&from, &to)
        })
        .map_err(|e| format!("restore {}: {e}", to.display()))?;
    }
    let _ = std::fs::remove_dir_all(backup);
    Ok(())
}

pub fn recover_interrupted(dir: &Path) -> Result<(), String> {
    let marker_path = dir.join(consts::UPDATE_STATE_NAME);
    let backup = dir.join(consts::BACKUP_DIR_NAME);
    if !(marker_path.exists() && backup.exists()) {
        return Ok(());
    }
    restore_backup(dir, &backup)?;
    let _ = std::fs::remove_file(&marker_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "peebify-swap-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staged_files() -> Vec<String> {
        vec![
            consts::MAIN_BINARY.to_string(),
            "resources/7z.exe".to_string(),
            "icons/app.png".to_string(),
        ]
    }

    #[test]
    fn foreign_folder_with_matching_names_collides() {
        let dir = scratch("foreign");
        std::fs::create_dir_all(dir.join("Resources")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        assert_eq!(foreign_collisions(&dir, &staged_files()), vec!["Resources"]);
        std::fs::create_dir_all(dir.join("icons")).unwrap();
        assert_eq!(
            foreign_collisions(&dir, &staged_files()),
            vec!["Resources", "icons"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn earlier_install_or_unrelated_folder_does_not_collide() {
        let dir = scratch("ours");
        std::fs::create_dir_all(dir.join("resources")).unwrap();
        std::fs::write(dir.join(consts::MAIN_BINARY), b"x").unwrap();
        assert!(foreign_collisions(&dir, &staged_files()).is_empty());

        let other = scratch("unrelated");
        std::fs::create_dir_all(other.join("tools")).unwrap();
        std::fs::create_dir_all(other.join(consts::GAMES_DIR_NAME)).unwrap();
        assert!(foreign_collisions(&other, &staged_files()).is_empty());
        assert!(foreign_collisions(&other.join("missing"), &staged_files()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn rollback_after_a_partial_swap_removes_new_entries_and_restores_old_ones() {
        let dir = scratch("partial-swap");
        let staged = dir.join(consts::STAGED_DIR_NAME);
        let backup = dir.join(consts::BACKUP_DIR_NAME);
        std::fs::write(dir.join(consts::MAIN_BINARY), b"old").unwrap();
        std::fs::write(dir.join("notes.txt"), b"mine").unwrap();
        std::fs::create_dir_all(staged.join("resources")).unwrap();
        std::fs::write(staged.join("resources").join("new.dll"), b"new").unwrap();
        std::fs::write(staged.join(consts::MAIN_BINARY), b"new").unwrap();
        std::fs::write(staged.join(consts::INSTALL_MANIFEST_NAME), b"{}").unwrap();
        let replaceable = vec![consts::MAIN_BINARY.to_lowercase()];

        let mut installed = Vec::new();
        swap(&dir, &staged, &backup, &replaceable, &mut installed).unwrap();
        assert_eq!(installed.len(), 3);
        std::fs::remove_file(dir.join(consts::INSTALL_MANIFEST_NAME)).unwrap();
        installed.retain(|path| !path.ends_with(consts::INSTALL_MANIFEST_NAME));
        assert_eq!(installed.len(), 2);

        roll_back_swap(&dir, &backup, &installed).unwrap();

        assert_eq!(std::fs::read(dir.join(consts::MAIN_BINARY)).unwrap(), b"old");
        assert!(!dir.join("resources").exists());
        assert!(dir.join("notes.txt").exists());
        assert!(!backup.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_reports_a_new_entry_it_could_not_remove() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("partial-swap-locked");
        let backup = dir.join(consts::BACKUP_DIR_NAME);
        std::fs::create_dir_all(&backup).unwrap();
        let stuck = dir.join(consts::UNINSTALLER_NAME);
        std::fs::write(&stuck, b"new").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&stuck)
            .unwrap();

        let result = roll_back_swap(&dir, &backup, std::slice::from_ref(&stuck));
        drop(lock);

        assert!(result.is_err());
        assert!(!backup.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
