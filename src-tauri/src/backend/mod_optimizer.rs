// ------------ Mods Folder Clean-up ------------
// Runs over a game's Mods folder before each modded launch and switches off files that hurt the mod loader: stray
// copies of its own config, helper files, duplicates of libraries the package already ships, and triggers the loader
// already fires itself. Edited files keep a backup next to them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::fs_util;

const SCAN_DEPTH: usize = 12;
const ROGUE_NAMES: [&str; 2] = ["d3dx.ini", "d3dx_user.ini"];
const ROGUE_SECTIONS: [&str; 4] = ["[loader", "[system", "[stereo", "[commandlistunbindallrendertargets"];
const ROGUE_INCLUDES: [(&str, &str); 2] = [("include_recursive", "mods"), ("exclude_recursive", "disabled*")];
const SHADERFIXES_DIR: &str = "shaderfixes";
const BACKUP_SUFFIX: &str = ".bak";

#[derive(Default)]
pub struct Rules {
    unwanted_triggers: Vec<String>,
    ib_replacement: Option<String>,
    unwanted_files: Vec<String>,
    libraries: Option<PathBuf>,
}

impl Rules {
    pub fn of(profile: &Value, variant_dir: &Path) -> Rules {
        let mods = profile.get("mods");
        let strings = |key: &str| -> Vec<String> {
            mods.and_then(|m| m.get(key))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_ascii_lowercase)
                        .collect()
                })
                .unwrap_or_default()
        };
        let libraries = mods
            .and_then(|m| m.get("bundledLibraries"))
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .fold(variant_dir.to_path_buf(), |path, part| path.join(part))
            });
        Rules {
            unwanted_triggers: strings("unwantedTriggers"),
            ib_replacement: mods
                .and_then(|m| m.get("ibTriggerReplacement"))
                .and_then(Value::as_str)
                .map(str::to_string),
            unwanted_files: strings("unwantedModFiles"),
            libraries,
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Report {
    pub stray_configs: usize,
    pub unwanted_files: usize,
    pub duplicate_libraries: usize,
    pub fixed_files: usize,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        *self == Report::default()
    }

    pub fn summary(&self, variant: &str) -> String {
        let count = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        let mut parts = Vec::new();
        if self.stray_configs > 0 {
            parts.push(format!(
                "turned off {} that only the mod tools should have",
                count(self.stray_configs, "stray loader config", "stray loader configs")
            ));
        }
        if self.duplicate_libraries > 0 {
            parts.push(format!(
                "turned off {} that {} already includes",
                count(self.duplicate_libraries, "duplicate library", "duplicate libraries"),
                variant.to_uppercase()
            ));
        }
        if self.unwanted_files > 0 {
            parts.push(format!(
                "turned off {}",
                count(self.unwanted_files, "helper file", "helper files")
            ));
        }
        if self.fixed_files > 0 {
            parts.push(format!(
                "fixed {} that slow the game down (backups kept as .bak)",
                count(self.fixed_files, "mod file", "mod files")
            ));
        }
        format!("Peebify {}.", parts.join(", "))
    }
}

pub fn is_rogue_config(text: &str) -> bool {
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let lower = line.to_ascii_lowercase();
            if ROGUE_SECTIONS.iter().any(|s| lower.starts_with(s)) {
                return true;
            }
            section = lower[1..]
                .split(']')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            continue;
        }
        if section != "include" {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_ascii_lowercase());
            if ROGUE_INCLUDES.iter().any(|(k, v)| *k == key && *v == value) {
                return true;
            }
        }
    }
    false
}

fn namespace_of(text: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let value = value.trim().to_ascii_lowercase();
        return (key.trim().eq_ignore_ascii_case("namespace") && !value.is_empty()).then_some(value);
    }
    None
}

fn fix_triggers(text: &str, rules: &Rules) -> Option<String> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut changed = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            let Some((key, value)) = trimmed.split_once('=') else {
                return line.to_string();
            };
            let slot = value.trim().to_ascii_lowercase();
            if trimmed.starts_with(';')
                || !key.trim().eq_ignore_ascii_case("checktextureoverride")
                || !rules.unwanted_triggers.contains(&slot)
            {
                return line.to_string();
            }
            changed = true;
            let indent = &line[..line.len() - line.trim_start().len()];
            match &rules.ib_replacement {
                Some(replacement) if slot == "ib" => format!("{indent}{replacement}"),
                _ => format!("{indent};{trimmed}"),
            }
        })
        .collect();
    if !changed {
        return None;
    }
    let mut out = lines.join(newline);
    if text.ends_with('\n') {
        out.push_str(newline);
    }
    Some(out)
}

fn ini_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_uppercase();
        if name.starts_with("DISABLED") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                ini_files(&path, depth - 1, out);
            }
        } else if name.ends_with(".INI") {
            out.push(path);
        }
    }
}

fn free_path(dir: &Path, name_for: impl Fn(Option<u32>) -> String) -> PathBuf {
    std::iter::once(None)
        .chain((2..1000).map(Some))
        .map(|n| dir.join(name_for(n)))
        .find(|path| !path.exists())
        .unwrap_or_else(|| dir.join(name_for(None)))
}

fn disable(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let name = name.to_string_lossy();
    let target = free_path(dir, |n| match n {
        Some(n) => format!("DISABLED_{n}_{name}"),
        None => format!("DISABLED_{name}"),
    });
    match std::fs::rename(path, &target) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("mods: could not turn off {}: {e}", path.display());
            false
        }
    }
}

fn backup_and_write(path: &Path, original: &[u8], updated: &str) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let name = name.to_string_lossy();
    let backup = free_path(dir, |n| match n {
        Some(n) => format!("{name}.{n}{BACKUP_SUFFIX}"),
        None => format!("{name}{BACKUP_SUFFIX}"),
    });
    if let Err(e) = std::fs::write(&backup, original) {
        log::warn!("mods: could not back up {}: {e}", path.display());
        return false;
    }
    match fs_util::write_atomic(path, updated.as_bytes()) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("mods: could not fix {}: {e}", path.display());
            false
        }
    }
}

fn library_namespaces(dir: &Path) -> HashSet<String> {
    let mut files = Vec::new();
    ini_files(dir, SCAN_DEPTH, &mut files);
    files
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| namespace_of(&String::from_utf8_lossy(&bytes)))
        .collect()
}

pub fn tidy(mods_dir: &Path, rules: &Rules) -> Report {
    let mut report = Report::default();
    let libraries = rules
        .libraries
        .as_deref()
        .filter(|dir| dir.is_dir())
        .map(library_namespaces)
        .unwrap_or_default();
    let mut files = Vec::new();
    ini_files(mods_dir, SCAN_DEPTH, &mut files);

    for path in files {
        let relative = path.strip_prefix(mods_dir).unwrap_or(&path).display().to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let folder = path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        if ROGUE_NAMES.contains(&name.as_str()) {
            if disable(&path) {
                log::warn!("mods: turned off the stray loader config {relative}");
                report.stray_configs += 1;
            }
            continue;
        }
        let helper = folder == SHADERFIXES_DIR
            && super::xxmi::SHADERFIXES_EXTRAS
                .iter()
                .any(|extra| extra.eq_ignore_ascii_case(&name));
        if helper || rules.unwanted_files.contains(&name) {
            if disable(&path) {
                log::info!("mods: turned off the helper file {relative}");
                report.unwanted_files += 1;
            }
            continue;
        }

        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if is_rogue_config(&text) {
            if disable(&path) {
                log::warn!("mods: turned off {relative}, a copy of the mod loader's own config");
                report.stray_configs += 1;
            }
            continue;
        }
        if namespace_of(&text).is_some_and(|ns| libraries.contains(&ns)) {
            if disable(&path) {
                log::info!("mods: turned off {relative}, a library the mod tools already include");
                report.duplicate_libraries += 1;
            }
            continue;
        }
        if rules.unwanted_triggers.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Some(fixed) = fix_triggers(text, rules) {
            if backup_and_write(&path, &bytes, &fixed) {
                log::info!("mods: switched off triggers the mod tools already fire in {relative}");
                report.fixed_files += 1;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fs_util::TempDir;

    fn rules() -> Rules {
        Rules {
            unwanted_triggers: vec!["ib".into(), "vb0".into()],
            ib_replacement: Some(r"$\WWMIv1\enable_ib_callbacks = 1".into()),
            unwanted_files: vec!["vscheck.ini".into()],
            libraries: None,
        }
    }

    #[test]
    fn loader_configs_are_recognised_by_their_sections_and_includes() {
        assert!(is_rogue_config("; note\n[Loader]\ntarget = Game.exe\n"));
        assert!(is_rogue_config("[CommandListUnbindAllRenderTargets]\n"));
        assert!(is_rogue_config("[Include]\ninclude_recursive = Mods\n"));
        assert!(is_rogue_config("\u{feff}[Stereo]\n"));
        assert!(!is_rogue_config("; [Loader]\n[TextureOverrideBody]\nhash = 1\n"));
        assert!(!is_rogue_config("[Constants]\ninclude_recursive = Mods\n"));
    }

    #[test]
    fn namespaces_come_from_the_first_real_line() {
        assert_eq!(namespace_of("; lib\n\nnamespace = GIMI\\Offset\n").as_deref(), Some("gimi\\offset"));
        assert_eq!(namespace_of("[Constants]\nnamespace = x\n"), None);
    }

    #[test]
    fn unwanted_triggers_are_commented_or_replaced() {
        let text = "[TextureOverrideBody]\r\n  checktextureoverride = ib\r\nCheckTextureOverride = vb0\r\n;checktextureoverride = ib\r\ncheckTextureOverride = ps-t0\r\n";
        assert_eq!(
            fix_triggers(text, &rules()).unwrap(),
            "[TextureOverrideBody]\r\n  $\\WWMIv1\\enable_ib_callbacks = 1\r\n;CheckTextureOverride = vb0\r\n;checktextureoverride = ib\r\ncheckTextureOverride = ps-t0\r\n"
        );
        let plain = Rules { ib_replacement: None, ..rules() };
        assert_eq!(fix_triggers("checktextureoverride = ib\n", &plain).unwrap(), ";checktextureoverride = ib\n");
        assert_eq!(fix_triggers("checktextureoverride = vs-cb0\n", &plain), None);
    }

    #[test]
    fn tidy_turns_off_bad_files_and_fixes_the_rest() {
        let dir = TempDir::new("mod-tidy");
        let mods = dir.path().join("Mods");
        let libs = dir.path().join("Core").join("GIMI").join("Libraries");
        std::fs::create_dir_all(mods.join("Cool").join("ShaderFixes")).unwrap();
        std::fs::create_dir_all(mods.join("DISABLED Old")).unwrap();
        std::fs::create_dir_all(&libs).unwrap();
        std::fs::write(libs.join("offset.ini"), "namespace = GIMI\\Offset\n").unwrap();
        std::fs::write(mods.join("Cool").join("mod.ini"), "[TextureOverrideA]\ncheckTextureOverride = ib\n").unwrap();
        std::fs::write(mods.join("Cool").join("d3dx.ini"), "[TextureOverrideA]\n").unwrap();
        std::fs::write(mods.join("Cool").join("copy.ini"), "[Loader]\n").unwrap();
        std::fs::write(mods.join("Cool").join("VSCheck.ini"), "x\n").unwrap();
        std::fs::write(mods.join("Cool").join("ShaderFixes").join("help.ini"), "x\n").unwrap();
        std::fs::write(mods.join("Cool").join("offset.ini"), "namespace = gimi\\offset\n").unwrap();
        std::fs::write(mods.join("DISABLED Old").join("d3dx.ini"), "[Loader]\n").unwrap();
        std::fs::write(mods.join("Cool").join("DISABLED_d3dx.ini"), "[Loader]\n").unwrap();

        let rules = Rules { libraries: Some(libs), ..rules() };
        let report = tidy(&mods, &rules);
        assert_eq!(
            report,
            Report { stray_configs: 2, unwanted_files: 2, duplicate_libraries: 1, fixed_files: 1 }
        );
        let cool = mods.join("Cool");
        assert!(cool.join("DISABLED_2_d3dx.ini").exists());
        assert!(cool.join("DISABLED_copy.ini").exists());
        assert!(cool.join("DISABLED_VSCheck.ini").exists());
        assert!(cool.join("ShaderFixes").join("DISABLED_help.ini").exists());
        assert!(cool.join("DISABLED_offset.ini").exists());
        assert!(mods.join("DISABLED Old").join("d3dx.ini").exists());
        assert_eq!(
            std::fs::read_to_string(cool.join("mod.ini")).unwrap(),
            "[TextureOverrideA]\n$\\WWMIv1\\enable_ib_callbacks = 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(cool.join("mod.ini.bak")).unwrap(),
            "[TextureOverrideA]\ncheckTextureOverride = ib\n"
        );
        assert!(tidy(&mods, &rules).is_empty());
    }

    #[test]
    fn the_summary_names_each_kind_of_change() {
        let report = Report { stray_configs: 1, unwanted_files: 0, duplicate_libraries: 2, fixed_files: 3 };
        assert_eq!(
            report.summary("gimi"),
            "Peebify turned off 1 stray loader config that only the mod tools should have, turned off 2 duplicate libraries that GIMI already includes, fixed 3 mod files that slow the game down (backups kept as .bak)."
        );
    }
}
