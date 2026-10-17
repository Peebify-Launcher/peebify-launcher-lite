// ------------ Mod Profiles ------------
// Named sets of mods per game. Applying a profile turns on the mods in it and turns off the rest, so players can
// switch between setups in one click. Profiles live in mods/profiles.json in the launcher's data folder.

use std::path::PathBuf;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::BackendState;
use super::{arg_str, err_response, game_profiles, mods, ok_with, resolve_profile};

const PROFILES_FILE: &str = "profiles.json";
pub(crate) const MAX_PROFILES_PER_GAME: usize = 50;
const MAX_NAME_LEN: usize = 60;
const MAX_MODS_PER_PROFILE: usize = 2000;

pub(crate) static PROFILES_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn profiles_root(app: &AppHandle) -> PathBuf {
    app.state::<BackendState>().user_data.join("mods")
}

fn profiles_path(app: &AppHandle) -> PathBuf {
    profiles_root(app).join(PROFILES_FILE)
}

pub(crate) fn load_store(app: &AppHandle) -> Result<Value, String> {
    super::fs_util::read_json_store(&profiles_path(app))
        .map(|stored| stored.filter(Value::is_object).unwrap_or_else(|| json!({})))
}

pub(crate) fn supports_mods(game_id: &str) -> bool {
    game_profiles::known_profile(game_id)
        .and_then(game_profiles::mod_config)
        .is_some()
}

fn moddable_only(store: &Value) -> Value {
    let kept: serde_json::Map<String, Value> = store
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(game_id, _)| supports_mods(game_id))
                .map(|(game_id, entry)| (game_id.clone(), entry.clone()))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(kept)
}

pub(crate) fn write_store(app: &AppHandle, store: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(&moddable_only(store)).map_err(|e| e.to_string())?;
    super::fs_util::write_atomic(&profiles_path(app), body.as_bytes())
}

pub(crate) fn clean_name(raw: &str) -> String {
    let trimmed: String = raw.trim().chars().take(MAX_NAME_LEN).collect();
    if trimmed.is_empty() {
        "Profile".to_string()
    } else {
        trimmed
    }
}

pub(crate) fn profiles_of(store: &Value, game_id: &str) -> Vec<Value> {
    store[game_id]["profiles"]
        .as_array()
        .map(|profiles| profiles.iter().filter(|p| p.is_object()).cloned().collect())
        .unwrap_or_default()
}

pub(crate) fn find_index(profiles: &[Value], profile_id: &str) -> Option<usize> {
    profiles
        .iter()
        .position(|p| p["id"].as_str() == Some(profile_id))
}

pub(crate) fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

pub(crate) fn mod_ids_of(profile: &Value) -> Vec<String> {
    string_list(Some(&profile["modIds"]))
}

fn new_profile(name: &str, mod_ids: Vec<String>) -> Value {
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "name": clean_name(name),
        "modIds": mod_ids,
        "createdAt": chrono::Utc::now().to_rfc3339(),
    })
}

pub(crate) fn strip_copy_suffix(name: &str) -> &str {
    let Some(open) = name.rfind(" (") else {
        return name;
    };
    match name[open + 2..].strip_suffix(')') {
        Some(number) if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) => {
            &name[..open]
        }
        _ => name,
    }
}

fn seeded_profiles(app: &AppHandle, game_id: &str, store: &mut Value) -> (Vec<Value>, String) {
    if !supports_mods(game_id) {
        return (Vec::new(), String::new());
    }
    let profiles = profiles_of(store, game_id);
    if !profiles.is_empty() {
        let active = store[game_id]["active"]
            .as_str()
            .filter(|id| find_index(&profiles, id).is_some())
            .map(str::to_string)
            .unwrap_or_else(|| profiles[0]["id"].as_str().unwrap_or_default().to_string());
        return (profiles, active);
    }

    let enabled: Vec<String> = mods::snapshot(app, game_id)
        .into_iter()
        .filter(|m| m.enabled)
        .map(|m| m.mod_id)
        .collect();

    let profile = new_profile("Default", enabled);
    let id = profile["id"].as_str().unwrap_or_default().to_string();
    store[game_id] = json!({ "active": id, "profiles": [profile.clone()] });
    if let Err(e) = write_store(app, store) {
        log::warn!("mod-profiles: could not seed a default profile for {game_id}: {e}");
    }
    (vec![profile], id)
}

pub(super) fn active_profile_name(app: &AppHandle, game_id: &str) -> Option<String> {
    let store = {
        let _guard = PROFILES_LOCK.lock();
        load_store(app).ok()?
    };
    let profiles = profiles_of(&store, game_id);
    let index = find_index(&profiles, store[game_id]["active"].as_str()?)?;
    profiles[index]["name"].as_str().map(str::to_string)
}

pub(crate) fn ensure_game(app: &AppHandle, game_id: &str) -> (Vec<Value>, String) {
    let _guard = PROFILES_LOCK.lock();
    match load_store(app) {
        Ok(mut store) => seeded_profiles(app, game_id, &mut store),
        Err(e) => {
            log::warn!("mod-profiles: could not load the profiles of {game_id}: {e}");
            (Vec::new(), String::new())
        }
    }
}

fn mutate<T>(
    app: &AppHandle,
    game_id: &str,
    edit: impl FnOnce(&mut Vec<Value>, &mut String) -> Result<T, String>,
) -> Result<T, String> {
    if !supports_mods(game_id) {
        return Err("This game does not support mods.".to_string());
    }
    let _guard = PROFILES_LOCK.lock();
    let mut store = load_store(app)?;
    let (mut profiles, mut active) = seeded_profiles(app, game_id, &mut store);

    let outcome = edit(&mut profiles, &mut active)?;

    if find_index(&profiles, &active).is_none() {
        active = profiles
            .first()
            .and_then(|p| p["id"].as_str())
            .unwrap_or_default()
            .to_string();
    }
    store[game_id] = json!({ "active": active, "profiles": profiles });
    write_store(app, &store)?;
    Ok(outcome)
}

fn game_id_from(app: &AppHandle, args: &[Value]) -> Result<String, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    if game_profiles::mod_config(profile).is_none() {
        return Err(format!(
            "{} does not support mods.",
            game_profiles::display_name(profile)
        ));
    }
    Ok(game_profiles::profile_id(profile).to_string())
}

fn target(app: &AppHandle, args: &[Value]) -> Result<(String, String), String> {
    let game_id = game_id_from(app, args)?;
    let profile_id = arg_str(args, 1).ok_or("No profile was specified.")?;
    Ok((game_id, profile_id.to_string()))
}

pub(super) fn forget_mod(app: &AppHandle, game_id: &str, mod_id: &str) {
    forget_mods(app, game_id, &[mod_id.to_string()]);
}

pub(super) fn forget_mods(app: &AppHandle, game_id: &str, mod_ids: &[String]) {
    if mod_ids.is_empty() {
        return;
    }
    let _guard = PROFILES_LOCK.lock();
    let mut store = match load_store(app) {
        Ok(store) => store,
        Err(e) => {
            log::warn!(
                "mod-profiles: could not drop {} mod(s) from {game_id}: {e}",
                mod_ids.len()
            );
            return;
        }
    };
    let Some(profiles) = store[game_id]["profiles"].as_array_mut() else {
        return;
    };
    let mut changed = false;
    for profile in profiles.iter_mut() {
        let Some(ids) = profile["modIds"].as_array_mut() else {
            continue;
        };
        let before = ids.len();
        ids.retain(|v| !v.as_str().is_some_and(|id| mod_ids.iter().any(|m| m == id)));
        changed |= ids.len() != before;
    }
    if changed {
        if let Err(e) = write_store(app, &store) {
            log::warn!(
                "mod-profiles: could not drop {} mod(s) from {game_id}: {e}",
                mod_ids.len()
            );
        }
    }
}

pub(super) fn add_to_active(app: &AppHandle, game_id: &str, mod_ids: &[String]) {
    if mod_ids.is_empty() {
        return;
    }
    let outcome = mutate(app, game_id, |profiles, active| {
        let Some(index) = find_index(profiles, active) else {
            return Err("No active profile.".to_string());
        };
        let mut ids = mod_ids_of(&profiles[index]);
        for id in mod_ids {
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.clone());
            }
        }
        profiles[index]["modIds"] = json!(ids);
        Ok(())
    });
    if let Err(e) = outcome {
        log::warn!(
            "mod-profiles: could not add {} mod(s) to the active profile for {game_id}: {e}",
            mod_ids.len()
        );
    }
}

pub(super) async fn list_mod_profiles(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let profile = resolve_profile(app, arg_str(args, 0));
    let game_id = game_profiles::profile_id(profile).to_string();
    if game_profiles::mod_config(profile).is_none() {
        return Ok(ok_with(json!({
            "gameId": game_id,
            "profiles": [],
            "activeId": "",
        })));
    }
    let (profiles, active) = ensure_game(app, &game_id);

    let present: std::collections::HashSet<String> = mods::snapshot(app, &game_id)
        .into_iter()
        .map(|m| m.mod_id)
        .collect();
    let profiles: Vec<Value> = profiles
        .into_iter()
        .map(|mut p| {
            let ids = mod_ids_of(&p);
            let resolved = ids.iter().filter(|id| present.contains(*id)).count();
            p["modCount"] = json!(ids.len());
            p["resolvedCount"] = json!(resolved);
            p
        })
        .collect();

    Ok(ok_with(json!({
        "gameId": game_id,
        "profiles": profiles,
        "activeId": active,
    })))
}

pub(super) async fn create_mod_profile(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let game_id = match game_id_from(app, args) {
        Ok(id) => id,
        Err(e) => return Ok(err_response(e)),
    };
    let name = arg_str(args, 1).unwrap_or("Profile").to_string();

    let created = mutate(app, &game_id, |profiles, _| {
        if profiles.len() >= MAX_PROFILES_PER_GAME {
            return Err(format!(
                "You can have at most {MAX_PROFILES_PER_GAME} profiles for one game."
            ));
        }
        let profile = new_profile(&name, Vec::new());
        profiles.push(profile.clone());
        Ok(profile)
    });

    Ok(match created {
        Ok(profile) => {
            log::info!(
                "mod-profiles: created {} \"{}\" for {game_id}",
                profile["id"].as_str().unwrap_or_default(),
                profile["name"].as_str().unwrap_or_default()
            );
            ok_with(json!({ "profile": profile }))
        }
        Err(e) => {
            log::warn!("mod-profiles: could not create a profile for {game_id}: {e}");
            err_response(e)
        }
    })
}

pub(super) async fn rename_mod_profile(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (game_id, profile_id) = match target(app, args) {
        Ok(found) => found,
        Err(e) => return Ok(err_response(e)),
    };
    let name = clean_name(arg_str(args, 2).unwrap_or_default());

    let outcome = mutate(app, &game_id, |profiles, _| {
        let Some(index) = find_index(profiles, &profile_id) else {
            return Err("That profile no longer exists.".to_string());
        };
        profiles[index]["name"] = json!(name);
        Ok(profiles[index].clone())
    });

    Ok(match outcome {
        Ok(profile) => {
            log::info!("mod-profiles: renamed {profile_id} to \"{name}\" for {game_id}");
            ok_with(json!({ "profile": profile }))
        }
        Err(e) => {
            log::warn!("mod-profiles: could not rename {profile_id} for {game_id}: {e}");
            err_response(e)
        }
    })
}

pub(super) async fn delete_mod_profile(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let (game_id, profile_id) = match target(app, args) {
        Ok(found) => found,
        Err(e) => return Ok(err_response(e)),
    };

    let outcome = mutate(app, &game_id, |profiles, _| {
        if profiles.len() <= 1 {
            return Err("You need at least one profile.".to_string());
        }
        let Some(index) = find_index(profiles, &profile_id) else {
            return Err("That profile no longer exists.".to_string());
        };
        profiles.remove(index);
        Ok(())
    });

    if let Err(e) = outcome {
        log::warn!("mod-profiles: could not delete {profile_id} for {game_id}: {e}");
        return Ok(err_response(e));
    }
    let (_, active) = ensure_game(app, &game_id);
    log::info!("mod-profiles: deleted {profile_id} for {game_id}, active is {active}");
    Ok(ok_with(json!({ "activeId": active })))
}

pub(super) async fn duplicate_mod_profile(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let (game_id, profile_id) = match target(app, args) {
        Ok(found) => found,
        Err(e) => return Ok(err_response(e)),
    };

    let outcome = mutate(app, &game_id, |profiles, _| {
        if profiles.len() >= MAX_PROFILES_PER_GAME {
            return Err(format!(
                "You can have at most {MAX_PROFILES_PER_GAME} profiles for one game."
            ));
        }
        let Some(index) = find_index(profiles, &profile_id) else {
            return Err("That profile no longer exists.".to_string());
        };
        let source = &profiles[index];
        let name = format!("{} copy", source["name"].as_str().unwrap_or("Profile"));
        let copy = new_profile(&name, mod_ids_of(source));
        profiles.insert(index + 1, copy.clone());
        Ok(copy)
    });

    Ok(match outcome {
        Ok(profile) => {
            log::info!(
                "mod-profiles: duplicated {profile_id} as {} for {game_id} with {} mod(s)",
                profile["id"].as_str().unwrap_or_default(),
                mod_ids_of(&profile).len()
            );
            ok_with(json!({ "profile": profile }))
        }
        Err(e) => {
            log::warn!("mod-profiles: could not duplicate {profile_id} for {game_id}: {e}");
            err_response(e)
        }
    })
}

pub(crate) async fn apply_mod_profile(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let game_id = match game_id_from(app, args) {
        Ok(id) => id,
        Err(e) => return Ok(err_response(e)),
    };
    let mods_dir = match mods::mods_dir_for(app, &game_id) {
        Ok(dir) => dir,
        Err(e) => return Ok(err_response(e)),
    };

    let (profiles, active) = ensure_game(app, &game_id);
    let profile_id = arg_str(args, 1).unwrap_or(&active).to_string();
    let Some(index) = find_index(&profiles, &profile_id) else {
        return Ok(err_response("That profile no longer exists."));
    };

    let wanted: std::collections::HashSet<String> =
        mod_ids_of(&profiles[index]).into_iter().collect();
    let present = mods::snapshot(app, &game_id);

    let on_disk: std::collections::HashSet<&str> =
        present.iter().map(|m| m.mod_id.as_str()).collect();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|id| !on_disk.contains(id.as_str()))
        .cloned()
        .collect();

    let plan: Vec<(String, bool)> = present
        .iter()
        .filter_map(|m| {
            let want = wanted.contains(&m.mod_id);
            (want != m.enabled).then(|| (m.folder.clone(), want))
        })
        .collect();

    let (changed, failed) = mods::apply_enabled(app, &game_id, &mods_dir, &plan).await;

    let set_active = mutate(app, &game_id, |profiles, active| {
        if find_index(profiles, &profile_id).is_none() {
            return Err("That profile no longer exists.".to_string());
        }
        *active = profile_id.clone();
        Ok(())
    });
    if let Err(e) = set_active {
        log::warn!("mod-profiles: applied {profile_id} but could not mark it active: {e}");
    }

    let enabled: Vec<&Value> = changed
        .iter()
        .filter(|c| c["enabled"] == Value::Bool(true))
        .collect();
    let disabled: Vec<&Value> = changed
        .iter()
        .filter(|c| c["enabled"] == Value::Bool(false))
        .collect();

    log::info!(
        "mod-profiles: applied \"{}\" ({profile_id}) for {game_id}: {} on, {} off, {} failed, {} missing",
        profiles[index]["name"].as_str().unwrap_or_default(),
        enabled.len(),
        disabled.len(),
        failed.len(),
        missing.len()
    );
    mods::notify_mods_changed(app, &game_id);

    Ok(ok_with(json!({
        "activeId": profile_id,
        "enabled": enabled,
        "disabled": disabled,
        "failed": failed,
        "missing": missing,
    })))
}

pub(super) async fn set_mod_profile_members(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let (game_id, profile_id) = match target(app, args) {
        Ok(found) => found,
        Err(e) => return Ok(err_response(e)),
    };
    let add = string_list(args.get(2));
    let remove = string_list(args.get(3));
    if add.is_empty() && remove.is_empty() {
        return Ok(err_response("Nothing to change."));
    }
    let known: std::collections::HashSet<String> = mods::snapshot(app, &game_id)
        .into_iter()
        .map(|m| m.mod_id)
        .collect();

    let outcome = mutate(app, &game_id, |profiles, _active| {
        let Some(index) = find_index(profiles, &profile_id) else {
            return Err("That profile no longer exists.".to_string());
        };
        let mut ids = mod_ids_of(&profiles[index]);
        ids.retain(|id| !remove.iter().any(|r| r == id));
        for id in &add {
            if known.contains(id) && !ids.iter().any(|existing| existing == id) {
                ids.push(id.clone());
            }
        }
        if ids.len() > MAX_MODS_PER_PROFILE {
            return Err(format!(
                "A profile can hold at most {MAX_MODS_PER_PROFILE} mods."
            ));
        }
        profiles[index]["modIds"] = json!(ids);
        Ok(profiles[index].clone())
    });

    Ok(match outcome {
        Ok(mut profile) => {
            let ids = mod_ids_of(&profile);
            log::info!(
                "mod-profiles: {game_id} {profile_id} members +{} -{} now {}",
                add.len(),
                remove.len(),
                ids.len()
            );
            profile["modCount"] = json!(ids.len());
            profile["resolvedCount"] = json!(ids.iter().filter(|id| known.contains(*id)).count());
            ok_with(json!({ "profile": profile }))
        }
        Err(e) => {
            log::warn!(
                "mod-profiles: could not change members of {profile_id} for {game_id}: {e}"
            );
            err_response(e)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_suffix_is_stripped_only_when_numeric() {
        assert_eq!(strip_copy_suffix("hat (2)"), "hat");
        assert_eq!(strip_copy_suffix("hat (red)"), "hat (red)");
        assert_eq!(strip_copy_suffix("hat ()"), "hat ()");
        assert_eq!(strip_copy_suffix("hat"), "hat");
    }

    #[test]
    fn games_without_mod_support_are_dropped_from_the_store() {
        let store = json!({
            "zzz": { "active": "p", "profiles": [] },
            "gf2": { "active": "q", "profiles": [] },
            "not-a-game": {},
        });
        let kept = moddable_only(&store);
        let keys: Vec<&String> = kept.as_object().expect("object").keys().collect();
        assert_eq!(keys, vec!["zzz"]);
        assert!(supports_mods("zzz"));
        assert!(!supports_mods("gf2"));
    }
}
