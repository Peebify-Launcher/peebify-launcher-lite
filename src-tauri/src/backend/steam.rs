// ------------ Steam Integration ------------
// For games that also exist on Steam. Detects whether an install came from Steam, finds steam.exe, and launches or
// uninstalls the game through it. Peebify installs, updates and repairs the Steam copy's files like any other install.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use regex::Regex;

use serde_json::Value;
use tokio::process::{Child, Command};

use super::process_utils::CREATE_NO_WINDOW;

pub fn app_id(profile: &Value) -> Option<&str> {
    profile.get("steamAppId")?.as_str()
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamSignal {
    Marker(PathBuf),
    Library(PathBuf),
}

impl std::fmt::Display for SteamSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marker(path) => write!(f, "marker {}", path.display()),
            Self::Library(path) => write!(f, "library manifest {}", path.display()),
        }
    }
}

pub fn detect(install_root: &Path, profile: &Value) -> Option<String> {
    detect_signal(install_root, profile).map(|(app_id, _)| app_id)
}

pub fn detect_signal(install_root: &Path, profile: &Value) -> Option<(String, SteamSignal)> {
    let expected = app_id(profile)?;
    let signal = matching_marker(install_root, profile, expected)
        .map(SteamSignal::Marker)
        .or_else(|| steam_library_manifest(install_root, expected).map(SteamSignal::Library))?;
    Some((expected.to_string(), signal))
}

fn matching_marker(install_root: &Path, profile: &Value, expected: &str) -> Option<PathBuf> {
    marker_paths(profile).into_iter().find_map(|rel| {
        let path = rel
            .iter()
            .fold(install_root.to_path_buf(), |p, part| p.join(part));
        std::fs::read_to_string(&path)
            .is_ok_and(|body| body.trim() == expected)
            .then_some(path)
    })
}

fn marker_paths(profile: &Value) -> Vec<Vec<String>> {
    profile
        .get("steamAppIdFiles")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|parts| {
                    Some(
                        parts
                            .as_array()?
                            .iter()
                            .filter_map(|p| p.as_str().map(str::to_string))
                            .collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn steam_library_manifest(install_root: &Path, expected: &str) -> Option<PathBuf> {
    let manifest = format!("appmanifest_{expected}.acf");
    let mut below: Option<&Path> = None;
    let mut dir = Some(install_root);
    while let Some(current) = dir {
        if is_named(current, "common") {
            if let Some(steamapps) = current.parent() {
                let path = steamapps.join(&manifest);
                if is_named(steamapps, "steamapps")
                    && path.is_file()
                    && manifest_owns_folder(&path, below)
                {
                    return Some(path);
                }
            }
        }
        below = Some(current);
        dir = current.parent();
    }
    None
}

fn manifest_owns_folder(manifest: &Path, folder: Option<&Path>) -> bool {
    let Some(folder) = folder.and_then(Path::file_name).and_then(|n| n.to_str()) else {
        return true;
    };
    let Ok(body) = std::fs::read_to_string(manifest) else {
        return true;
    };
    match manifest_install_dir(&body) {
        Some(dir) => dir.eq_ignore_ascii_case(folder),
        None => true,
    }
}

fn manifest_install_dir(body: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r#"(?i)"installdir"\s*"([^"]*)""#).expect("static regex"));
    re.captures(body)?.get(1).map(|m| m.as_str())
}

fn is_named(path: &Path, name: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(name))
}

pub fn steam_exe() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Valve\Steam")
        .ok();
    if let Some(key) = hkcu {
        for value in ["SteamExe", "SteamPath"] {
            if let Ok(raw) = key.get_value::<String, _>(value) {
                if let Some(exe) = steam_exe_from(&raw) {
                    return Some(exe);
                }
            }
        }
    }

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\Valve\Steam", KEY_READ | KEY_WOW64_32KEY)
        .ok()?;
    let raw = hklm.get_value::<String, _>("InstallPath").ok()?;
    steam_exe_from(&raw)
}

fn steam_exe_from(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim().replace('/', "\\");
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(&raw);
    let exe = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        path
    } else {
        path.join("steam.exe")
    };
    exe.is_file().then_some(exe)
}

pub async fn launch(steam_exe: &Path, app_id: &str, args: &[String]) -> Result<Child, String> {
    let mut argv = vec!["-applaunch".to_string(), app_id.to_string()];
    argv.extend_from_slice(args);
    spawn(steam_exe, &argv)
}

pub async fn request_uninstall(steam_exe: &Path, app_id: &str) -> Result<Child, String> {
    spawn(steam_exe, &[format!("steam://uninstall/{app_id}")])
}

fn spawn(steam_exe: &Path, args: &[String]) -> Result<Child, String> {
    let mut cmd = Command::new(steam_exe);
    cmd.args(args);
    if let Some(dir) = steam_exe.parent() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.creation_flags(CREATE_NO_WINDOW);

    cmd.spawn().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "peebify-steam-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn profile() -> Value {
        serde_json::json!({
            "steamAppId": "3513350",
            "steamAppIdFiles": [["Client", "steam_appid.txt"]],
        })
    }

    #[test]
    fn marker_signal_names_the_marker_file() {
        let root = scratch("marker");
        std::fs::create_dir_all(root.join("Client")).unwrap();
        std::fs::write(root.join("Client").join("steam_appid.txt"), "3513350\n").unwrap();
        let found = detect_signal(&root, &profile());
        assert_eq!(
            found,
            Some((
                "3513350".to_string(),
                SteamSignal::Marker(root.join("Client").join("steam_appid.txt"))
            ))
        );
        assert_eq!(detect(&root, &profile()), Some("3513350".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_signal_names_the_app_manifest() {
        let base = scratch("library");
        let steamapps = base.join("steamapps");
        let root = steamapps.join("common").join("Wuthering Waves");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(steamapps.join("appmanifest_3513350.acf"), "").unwrap();
        assert_eq!(
            detect_signal(&root, &profile()),
            Some((
                "3513350".to_string(),
                SteamSignal::Library(steamapps.join("appmanifest_3513350.acf"))
            ))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn library_signal_ignores_sibling_of_install_dir() {
        let base = scratch("sibling");
        let steamapps = base.join("steamapps");
        let steam_copy = steamapps.join("common").join("Wuthering Waves");
        let own_copy = steamapps.join("common").join("WuWa-Peebify");
        std::fs::create_dir_all(&steam_copy).unwrap();
        std::fs::create_dir_all(own_copy.join("Client")).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_3513350.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"3513350\"\n\t\"installdir\"\t\t\"Wuthering Waves\"\n}\n",
        )
        .unwrap();
        let manifest = steamapps.join("appmanifest_3513350.acf");
        assert_eq!(
            detect_signal(&steam_copy, &profile()),
            Some(("3513350".to_string(), SteamSignal::Library(manifest.clone())))
        );
        assert_eq!(
            detect_signal(&steam_copy.join("Client"), &profile()),
            Some(("3513350".to_string(), SteamSignal::Library(manifest)))
        );
        assert_eq!(detect_signal(&own_copy, &profile()), None);
        assert_eq!(detect_signal(&own_copy.join("Client"), &profile()), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn manifest_install_dir_is_case_insensitive_key() {
        assert_eq!(
            manifest_install_dir("\"AppState\" { \"InstallDir\" \"Game\" }"),
            Some("Game")
        );
        assert_eq!(manifest_install_dir("\"AppState\" { }"), None);
    }

    #[test]
    fn wrong_marker_and_no_manifest_is_not_steam() {
        let root = scratch("none");
        std::fs::create_dir_all(root.join("Client")).unwrap();
        std::fs::write(root.join("Client").join("steam_appid.txt"), "4162040").unwrap();
        assert_eq!(detect_signal(&root, &profile()), None);
        assert_eq!(detect(&root, &profile()), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
