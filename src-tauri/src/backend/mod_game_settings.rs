// ------------ Game Settings For Mods ------------
// Changes the few in-game settings that break modded characters, just before a modded launch: Genshin's Dynamic
// Character Resolution, Wuthering Waves' mesh LOD distance, ray tracing and wounded effect, and Zenless Zone Zero's
// character quality. Each game says which set it needs with `mods.gameSettings` in its profile.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::fs_util;

const ENABLED_KEY: &str = "modGameSettings";
const WOUNDED_FX_KEY: &str = "modNoWoundedFx";

pub struct Toggle {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

fn kind(profile: &Value) -> Option<&str> {
    profile.get("mods")?.get("gameSettings")?.as_str()
}

pub fn toggles(profile: &Value) -> Vec<Toggle> {
    let main = |description: &'static str| Toggle {
        key: ENABLED_KEY,
        label: "Adjust game settings for mods",
        description,
    };
    match kind(profile) {
        Some("genshin") => vec![main(
            "Turns Dynamic Character Resolution off before each modded launch. Modded characters render wrong while it is on.",
        )],
        Some("zzz") => vec![main(
            "Sets Character Quality to High and turns High-Precision Character Animation off before each modded launch. Mods only render with those settings.",
        )],
        Some("wuwa") => vec![
            main(
                "Keeps modded characters from switching to the plain model at a distance and turns ray tracing off, which does not work with mods.",
            ),
            Toggle {
                key: WOUNDED_FX_KEY,
                label: "Turn off the wounded effect",
                description: "Most mods' textures break after a few hits while the wounded effect is on. Leave this off if you use a mod that removes the effect.",
            },
        ],
        _ => Vec::new(),
    }
}

pub fn defaults(profile: &Value) -> Vec<(&'static str, Value)> {
    toggles(profile).iter().map(|t| (t.key, json!(true))).collect()
}

pub fn status(profile: &Value, config: &super::config::LauncherConfig, game_id: &str) -> Value {
    Value::Array(
        toggles(profile)
            .iter()
            .map(|t| {
                json!({
                    "key": t.key,
                    "label": t.label,
                    "description": t.description,
                    "value": config.get(&format!("games.{game_id}.{}", t.key)) != Value::Bool(false),
                })
            })
            .collect(),
    )
}

pub fn apply(
    profile: &Value,
    config: &super::config::LauncherConfig,
    game_id: &str,
    install_root: &Path,
    executable: &Path,
) -> Result<(), String> {
    let on = |key: &str| config.get(&format!("games.{game_id}.{key}")) != Value::Bool(false);
    if !on(ENABLED_KEY) {
        return Ok(());
    }
    match kind(profile) {
        Some("genshin") => genshin::apply(),
        Some("wuwa") => wuwa::apply(install_root, on(WOUNDED_FX_KEY)),
        Some("zzz") => match executable.parent() {
            Some(dir) => zzz::apply(dir),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

fn ascii_json(value: &Value) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

// ------------ Genshin Impact ------------
// Dynamic Character Resolution lives in a JSON registry value whose graphics parts are JSON strings of their own.
mod genshin {
    use serde_json::{json, Value};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_BINARY};
    use winreg::{RegKey, RegValue};

    const KEYS: [&str; 2] = [r"SOFTWARE\miHoYo\Genshin Impact", r"SOFTWARE\miHoYo\原神"];
    const VALUE: &str = "GENERAL_DATA_h2389025596";
    const DCR: i64 = 21;
    const ITEM_VERSION: &str = "OSRELWin5.0.0";
    const NOT_STARTED: &str =
        "Genshin Impact has no saved graphics settings yet. Start it once, then launch with mods again.";

    pub fn apply() -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = KEYS
            .iter()
            .find_map(|path| hkcu.open_subkey_with_flags(path, KEY_READ | KEY_SET_VALUE).ok())
            .ok_or(NOT_STARTED)?;
        let raw = key.get_raw_value(VALUE).map_err(|_| NOT_STARTED)?;
        if raw.vtype != REG_BINARY {
            return Err("Genshin Impact's graphics settings are in a format Peebify doesn't know.".into());
        }
        let end = raw.bytes.iter().position(|b| *b == 0).unwrap_or(raw.bytes.len());
        let mut settings: Value = serde_json::from_slice(&raw.bytes[..end])
            .map_err(|_| "Genshin Impact's graphics settings could not be read.".to_string())?;
        if !turn_off_dcr(&mut settings)? {
            return Ok(());
        }
        let mut bytes = super::ascii_json(&settings).into_bytes();
        bytes.push(0);
        key.set_raw_value(VALUE, &RegValue { bytes: bytes.into(), vtype: REG_BINARY })
            .map_err(|e| format!("Could not save Genshin Impact's graphics settings ({e})."))?;
        log::info!("mods: turned Genshin Impact's Dynamic Character Resolution off");
        Ok(())
    }

    fn nested(settings: &Value, key: &str) -> Result<Value, String> {
        settings
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or_else(|| format!("Genshin Impact's graphics settings have no readable {key}."))
    }

    fn list<'a>(value: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>, String> {
        value
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("Genshin Impact's graphics settings have no {key} list."))
    }

    pub(super) fn turn_off_dcr(settings: &mut Value) -> Result<bool, String> {
        let mut graphics = nested(settings, "graphicsData")?;
        let mut perf = nested(settings, "globalPerfData")?;
        let mut changed = false;

        let grades = list(&mut graphics, "customVolatileGrades")?;
        match grades.iter_mut().find(|entry| entry["key"] == DCR) {
            Some(entry) if entry["value"] == 2 => {
                entry["value"] = json!(1);
                changed = true;
            }
            Some(_) => {}
            None => {
                grades.push(json!({ "key": DCR, "value": 1 }));
                changed = true;
            }
        }

        let items = list(&mut perf, "saveItems")?;
        match items.iter_mut().find(|entry| entry["entryType"] == DCR) {
            Some(entry) if entry["index"] == 1 => {
                entry["index"] = json!(0);
                entry["itemVersion"] = json!(ITEM_VERSION);
                changed = true;
            }
            Some(_) => {}
            None => {
                items.push(json!({ "entryType": DCR, "index": 0, "itemVersion": ITEM_VERSION }));
                changed = true;
            }
        }

        if changed {
            settings["graphicsData"] = Value::String(super::ascii_json(&graphics));
            settings["globalPerfData"] = Value::String(super::ascii_json(&perf));
        }
        Ok(changed)
    }
}

// ------------ Wuthering Waves ------------
// The mesh LOD distance is an engine console variable in UserEngine.ini; ray tracing and the wounded effect are rows
// in the LocalStorage database.
mod wuwa {
    use std::path::Path;

    const SECTION: &str = "ConsoleVariables";
    const LOD_KEY: &str = "r.Kuro.SkeletalMesh.DistanceLODBaseFOV";
    const LOD_VALUE: &str = "165";
    const RAY_TRACING: [&str; 3] = ["RayTracing", "RayTracedReflection", "RayTracedGI"];
    const WOUNDED_FX: &str = "SkinDamageMode";

    pub fn apply(root: &Path, no_wounded_fx: bool) -> Result<(), String> {
        let client = root.join("Client");
        let user_engine = client.join("Config").join("UserEngine.ini");
        let text = std::fs::read_to_string(&user_engine).unwrap_or_default();
        let updated = set_option(&text, SECTION, LOD_KEY, LOD_VALUE);
        if updated != text {
            if let Some(dir) = user_engine.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
            }
            super::fs_util::write_atomic(&user_engine, updated.as_bytes())?;
            log::info!("mods: set {LOD_KEY}={LOD_VALUE} in UserEngine.ini");
        }

        let engine = client.join("Saved").join("Config").join("WindowsNoEditor").join("Engine.ini");
        if let Ok(text) = std::fs::read_to_string(&engine) {
            let updated = remove_option(&text, SECTION, LOD_KEY);
            if updated != text {
                super::fs_util::write_atomic(&engine, updated.as_bytes())?;
                log::info!("mods: removed a conflicting {LOD_KEY} from Engine.ini");
            }
        }

        let database = client.join("Saved").join("LocalStorage").join("LocalStorage.db");
        if database.is_file() {
            let mut values: Vec<(&str, &str)> = RAY_TRACING.iter().map(|key| (*key, "0")).collect();
            if no_wounded_fx {
                values.push((WOUNDED_FX, "0"));
            }
            set_rows(&database, &values)?;
        }
        Ok(())
    }

    fn header(line: &str) -> Option<&str> {
        let rest = line.trim().strip_prefix('[')?;
        Some(rest[..rest.find(']')?].trim())
    }

    fn key_of(line: &str) -> Option<&str> {
        let trimmed = line.trim();
        if trimmed.starts_with(';') || trimmed.starts_with('#') {
            return None;
        }
        Some(trimmed.split_once('=')?.0.trim())
    }

    fn section_range(lines: &[String], section: &str) -> Option<(usize, usize)> {
        let start = lines
            .iter()
            .position(|l| header(l).is_some_and(|name| name.eq_ignore_ascii_case(section)))?;
        let end = lines[start + 1..]
            .iter()
            .position(|l| header(l).is_some())
            .map_or(lines.len(), |offset| start + 1 + offset);
        Some((start, end))
    }

    fn join(lines: Vec<String>, original: &str) -> String {
        let newline = if original.contains("\r\n") { "\r\n" } else { "\n" };
        let mut out = lines.join(newline);
        if !out.is_empty() {
            out.push_str(newline);
        }
        out
    }

    pub(super) fn set_option(text: &str, section: &str, key: &str, value: &str) -> String {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let entry = format!("{key}={value}");
        match section_range(&lines, section) {
            Some((start, end)) => {
                let matching: Vec<usize> = (start + 1..end)
                    .filter(|&i| key_of(&lines[i]).is_some_and(|k| k.eq_ignore_ascii_case(key)))
                    .collect();
                match matching.split_first() {
                    Some((&first, rest)) => {
                        lines[first] = entry;
                        for &i in rest.iter().rev() {
                            lines.remove(i);
                        }
                    }
                    None => lines.insert(start + 1, entry),
                }
            }
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{section}]"));
                lines.push(entry);
            }
        }
        join(lines, text)
    }

    pub(super) fn remove_option(text: &str, section: &str, key: &str) -> String {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let Some((start, end)) = section_range(&lines, section) else {
            return text.to_string();
        };
        let before = lines.len();
        let mut i = end;
        while i > start + 1 {
            i -= 1;
            if key_of(&lines[i]).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
                lines.remove(i);
            }
        }
        if lines.len() == before {
            return text.to_string();
        }
        join(lines, text)
    }

    pub(super) fn set_rows(database: &Path, values: &[(&str, &str)]) -> Result<(), String> {
        let failed = |e: rusqlite::Error| format!("Could not update Wuthering Waves' settings database ({e}).");
        let connection = rusqlite::Connection::open(database).map_err(failed)?;
        connection.busy_timeout(super::Duration::from_secs(3)).map_err(failed)?;
        let has_table: bool = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'LocalStorage'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .map_err(failed)?;
        if !has_table {
            return Ok(());
        }
        for (key, value) in values {
            let current: Option<String> = connection
                .query_row("SELECT value FROM LocalStorage WHERE key = ?1", [key], |row| row.get(0))
                .ok();
            match current.as_deref() {
                Some(existing) if existing == *value => continue,
                Some(_) => connection
                    .execute("UPDATE LocalStorage SET value = ?1 WHERE key = ?2", [value, key])
                    .map_err(failed)?,
                None => connection
                    .execute("INSERT INTO LocalStorage (key, value) VALUES (?1, ?2)", [key, value])
                    .map_err(failed)?,
            };
            log::info!("mods: set Wuthering Waves' {key} to {value}");
        }
        Ok(())
    }
}

// ------------ Zenless Zone Zero ------------
// GENERAL_DATA.bin is a BinaryFormatter string wrapped around an XOR-encoded JSON text, written in the game's own
// JSON layout. The encoding is checked by decoding the result again before the file is replaced.
mod zzz {
    use std::path::Path;

    use serde_json::{json, Map, Value};

    const MAGIC: [u8; 29] = [
        85, 110, 209, 150, 116, 209, 131, 206, 149, 110, 103, 105, 110, 208, 181, 46, 71, 208, 176, 109, 101, 206,
        159, 98, 106, 101, 209, 129, 116,
    ];
    const HEADER: [u8; 22] = [0, 1, 0, 0, 0, 255, 255, 255, 255, 1, 0, 0, 0, 0, 0, 0, 0, 6, 1, 0, 0, 0];
    const FOOTER: u8 = 11;
    const INDENT: &str = "    ";
    const NEWLINE: &str = "\r\n";
    const SETTINGS: [(&str, i64); 3] = [("3", 3), ("13162", 0), ("99", 1)];

    pub fn apply(game_dir: &Path) -> Result<(), String> {
        let path = game_dir
            .join("ZenlessZoneZero_Data")
            .join("Persistent")
            .join("LocalStorage")
            .join("GENERAL_DATA.bin");
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(());
        };
        let unreadable = |_| "Zenless Zone Zero's settings file is in a format Peebify doesn't know.".to_string();
        let text = decode(&bytes)?;
        let mut settings: Value = serde_json::from_slice(&text).map_err(unreadable)?;
        if !set_system_settings(&mut settings)? {
            return Ok(());
        }
        let encoded = encode(game_json(&settings).as_bytes());
        let check: Value = serde_json::from_slice(&decode(&encoded)?).map_err(unreadable)?;
        if check != settings {
            return Err("Peebify could not write Zenless Zone Zero's settings safely, so it left them alone.".into());
        }
        let backup = path.with_file_name("GENERAL_DATA.bin.bak");
        if !backup.exists() {
            let _ = std::fs::write(&backup, &bytes);
        }
        super::fs_util::write_atomic(&path, &encoded)?;
        log::info!("mods: set Zenless Zone Zero's character quality for mods");
        Ok(())
    }

    fn evil(index: usize) -> bool {
        MAGIC[index % MAGIC.len()] & 0xC0 == 0xC0
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        let broken = || "Zenless Zone Zero's settings file is damaged or in a new format.".to_string();
        if !bytes.starts_with(&HEADER) {
            return Err(broken());
        }
        let mut at = HEADER.len();
        let mut length = 0usize;
        for shift in (0..35).step_by(7) {
            let byte = *bytes.get(at).ok_or_else(broken)?;
            at += 1;
            length |= ((byte & 0x7F) as usize) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let data = bytes.get(at..at + length).ok_or_else(broken)?;
        if bytes.get(at + length) != Some(&FOOTER) {
            return Err(broken());
        }
        let mut out = Vec::with_capacity(length);
        let mut shifted = false;
        for (i, byte) in data.iter().enumerate() {
            let ch = byte ^ MAGIC[i % MAGIC.len()];
            if evil(i) {
                shifted = ch != 0;
            } else {
                out.push(if std::mem::take(&mut shifted) { ch.wrapping_add(0x40) } else { ch });
            }
        }
        Ok(out)
    }

    pub(super) fn encode(content: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(content.len() * 2);
        let mut i = 0usize;
        for &byte in content {
            let mut ch = byte;
            if evil(i) {
                let flag = if ch > 0x40 {
                    ch -= 0x40;
                    1
                } else {
                    0
                };
                data.push(flag ^ MAGIC[i % MAGIC.len()]);
                i += 1;
            }
            data.push(ch ^ MAGIC[i % MAGIC.len()]);
            i += 1;
        }
        let mut out = HEADER.to_vec();
        let mut length = data.len();
        while length >= 0x80 {
            out.push((length as u8 & 0x7F) | 0x80);
            length >>= 7;
        }
        out.push(length as u8);
        out.extend_from_slice(&data);
        out.push(FOOTER);
        out
    }

    pub(super) fn set_system_settings(settings: &mut Value) -> Result<bool, String> {
        let root = settings
            .as_object_mut()
            .ok_or("Zenless Zone Zero's settings file has no settings in it.")?;
        let map = root
            .entry("SystemSettingDataMap")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or("Zenless Zone Zero's settings file has an unknown layout.")?;
        let mut changed = false;
        for (id, value) in SETTINGS {
            match map.get_mut(id) {
                Some(entry) => {
                    let data = entry
                        .get_mut("Data")
                        .ok_or("Zenless Zone Zero's settings file has an unknown layout.")?;
                    if *data != value {
                        *data = json!(value);
                        changed = true;
                    }
                }
                None => {
                    map.insert(
                        id.to_string(),
                        json!({ "$Type": "MoleMole.SystemSettingLocalData", "Version": 0, "Data": value }),
                    );
                    changed = true;
                }
            }
        }
        Ok(changed)
    }

    pub(super) fn game_json(value: &Value) -> String {
        format!("{NEWLINE}{}", dump(value, 0))
    }

    fn dump(value: &Value, level: usize) -> String {
        match value {
            Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(items) => {
                let mut out = format!("[{NEWLINE}");
                for (i, item) in items.iter().enumerate() {
                    let separator = if i + 1 == items.len() { "" } else { "," };
                    out.push_str(&format!("{}{}{separator}{NEWLINE}", INDENT.repeat(level + 1), dump(item, level + 1)));
                }
                out.push_str(&format!("{}]", INDENT.repeat(level)));
                out
            }
            Value::Object(map) => {
                let mut out = format!("{{{NEWLINE}");
                let mut widest = 0;
                for (i, (key, item)) in map.iter().enumerate() {
                    let width = key.chars().count();
                    widest = widest.max(width);
                    let separator = if i + 1 == map.len() { "" } else { "," };
                    out.push_str(&format!(
                        "{}\"{key}\"{}: {}{separator}{NEWLINE}",
                        INDENT.repeat(level + 1),
                        " ".repeat(widest - width + 1),
                        dump(item, level + 1)
                    ));
                }
                out.push_str(&format!("{}}}", INDENT.repeat(level)));
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genshin_dcr_is_turned_off_once() {
        let graphics = r#"{"customVolatileGrades":[{"key":21,"value":2},{"key":3,"value":"é"}]}"#;
        let perf = r#"{"saveItems":[{"entryType":21,"index":1,"itemVersion":"x"}]}"#;
        let mut settings = json!({ "graphicsData": graphics, "globalPerfData": perf, "other": 1 });
        assert!(genshin::turn_off_dcr(&mut settings).unwrap());
        assert_eq!(
            settings["graphicsData"],
            r#"{"customVolatileGrades":[{"key":21,"value":1},{"key":3,"value":"~u00e9"}]}"#.replace('~', "\\")
        );
        assert_eq!(
            settings["globalPerfData"],
            r#"{"saveItems":[{"entryType":21,"index":0,"itemVersion":"OSRELWin5.0.0"}]}"#
        );
        assert!(!genshin::turn_off_dcr(&mut settings).unwrap());
        assert!(ascii_json(&settings).is_ascii());
    }

    #[test]
    fn genshin_adds_missing_dcr_entries() {
        let mut settings = json!({
            "graphicsData": r#"{"customVolatileGrades":[]}"#,
            "globalPerfData": r#"{"saveItems":[]}"#,
        });
        assert!(genshin::turn_off_dcr(&mut settings).unwrap());
        assert_eq!(settings["graphicsData"], r#"{"customVolatileGrades":[{"key":21,"value":1}]}"#);
        assert!(genshin::turn_off_dcr(&mut json!({ "graphicsData": "{}" })).is_err());
    }

    #[test]
    fn engine_ini_option_is_set_once_and_duplicates_go() {
        let text = "[Core.System]\r\nPaths=x\r\n[ConsoleVariables]\r\nr.Kuro.SkeletalMesh.DistanceLODBaseFOV=90\r\nr.Other=1\r\nr.kuro.skeletalmesh.distancelodbasefov=30\r\n";
        assert_eq!(
            wuwa::set_option(text, "ConsoleVariables", "r.Kuro.SkeletalMesh.DistanceLODBaseFOV", "165"),
            "[Core.System]\r\nPaths=x\r\n[ConsoleVariables]\r\nr.Kuro.SkeletalMesh.DistanceLODBaseFOV=165\r\nr.Other=1\r\n"
        );
        assert_eq!(
            wuwa::set_option("", "ConsoleVariables", "r.A", "1"),
            "[ConsoleVariables]\nr.A=1\n"
        );
        assert_eq!(
            wuwa::set_option("[Other]\nx=1\n", "ConsoleVariables", "r.A", "1"),
            "[Other]\nx=1\n\n[ConsoleVariables]\nr.A=1\n"
        );
        assert_eq!(
            wuwa::remove_option("[ConsoleVariables]\nr.A=1\nr.B=2\n[X]\nr.A=3\n", "ConsoleVariables", "r.a"),
            "[ConsoleVariables]\nr.B=2\n[X]\nr.A=3\n"
        );
        assert_eq!(wuwa::remove_option("[X]\nr.A=1\n", "ConsoleVariables", "r.A"), "[X]\nr.A=1\n");
    }

    #[test]
    fn wuwa_rows_are_updated_or_added() {
        let dir = fs_util::TempDir::new("wuwa-settings");
        let path = dir.path().join("LocalStorage.db");
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE LocalStorage(key text primary key not null, value text not null); INSERT INTO LocalStorage VALUES ('RayTracing', '1'), ('Other', '5');",
                )
                .unwrap();
        }
        wuwa::set_rows(&path, &[("RayTracing", "0"), ("SkinDamageMode", "0")]).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let read = |key: &str| -> String {
            connection
                .query_row("SELECT value FROM LocalStorage WHERE key = ?1", [key], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(read("RayTracing"), "0");
        assert_eq!(read("SkinDamageMode"), "0");
        assert_eq!(read("Other"), "5");
    }

    #[test]
    fn zzz_codec_round_trips_and_matches_the_reference_layout() {
        let settings = json!({
            "$Type": "MoleMole.GeneralLocalDataItem",
            "userLocalDataVersionId": "0.0.1",
            "SystemSettingDataMap": { "3": { "$Type": "MoleMole.SystemSettingLocalData", "Version": 0, "Data": 2 } },
            "Empty": [],
            "Flags": [true, null, 1.5],
            "Quoted": "a\"b\\c",
        });
        let text = zzz::game_json(&settings);
        assert!(text.starts_with("\r\n{\r\n    \"$Type\" : \"MoleMole.GeneralLocalDataItem\",\r\n"));
        assert!(text.contains("    \"userLocalDataVersionId\" : \"0.0.1\",\r\n"));
        assert!(text.contains("    \"Empty\"                  : [\r\n    ],\r\n"));
        let encoded = zzz::encode(text.as_bytes());
        assert_eq!(encoded.last(), Some(&11));
        assert_eq!(zzz::decode(&encoded).unwrap(), text.as_bytes());
        let parsed: Value = serde_json::from_slice(&zzz::decode(&encoded).unwrap()).unwrap();
        assert_eq!(parsed, settings);
        assert!(zzz::decode(&encoded[..encoded.len() - 1]).is_err());
        assert!(zzz::decode(b"not a settings file at all").is_err());
    }

    #[test]
    fn zzz_settings_are_set_and_added() {
        let mut settings = json!({
            "SystemSettingDataMap": {
                "3": { "$Type": "MoleMole.SystemSettingLocalData", "Version": 0, "Data": 2 },
                "99": { "Data": 1 },
            }
        });
        assert!(zzz::set_system_settings(&mut settings).unwrap());
        let map = &settings["SystemSettingDataMap"];
        assert_eq!(map["3"]["Data"], 3);
        assert_eq!(map["13162"]["Data"], 0);
        assert_eq!(map["13162"]["$Type"], "MoleMole.SystemSettingLocalData");
        assert!(!zzz::set_system_settings(&mut settings).unwrap());
        assert!(zzz::set_system_settings(&mut json!({ "SystemSettingDataMap": { "3": {} } })).is_err());
    }

    #[test]
    fn games_without_settings_have_no_toggles() {
        assert!(toggles(&json!({ "mods": { "variant": "efmi" } })).is_empty());
        assert_eq!(toggles(&json!({ "mods": { "gameSettings": "wuwa" } })).len(), 2);
    }
}
