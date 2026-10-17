// ------------ Launcher Build Script ------------
// Makes the build fail if Cargo.toml, package.json and tauri.conf.json disagree on the version, passes the
// beta/stable build type to the compiler and embeds the Windows manifest that lifts the MAX_PATH limit.

use std::{env, fs, path::Path};

const APP_MANIFEST: &str = "windows-app.manifest";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PEEBIFY_BUILD_TYPE");
    println!("cargo:rerun-if-changed=../package.json");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed={APP_MANIFEST}");

    assert_versions_agree();

    if env::var("PEEBIFY_BUILD_TYPE").is_err() {
        if let Some(build_type) = build_type_from_package() {
            println!("cargo:rustc-env=PEEBIFY_BUILD_TYPE={build_type}");
        }
    }

    let attributes = match fs::read_to_string(APP_MANIFEST) {
        Ok(manifest) => tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new().app_manifest(manifest)),
        Err(e) => {
            if env::var("PROFILE").as_deref() == Ok("release") {
                panic!("could not read {APP_MANIFEST} ({e}); a release build without it would be capped at MAX_PATH");
            }
            println!("cargo:warning=could not read {APP_MANIFEST} ({e}); using Tauri's default manifest, so the launcher will be capped at MAX_PATH");
            tauri_build::Attributes::new()
        }
    };
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}

fn build_type_from_package() -> Option<String> {
    let raw = fs::read_to_string(Path::new("../package.json")).ok()?;
    let package: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let build_type = package.get("buildType")?.as_str()?;
    matches!(build_type, "beta" | "stable").then(|| build_type.to_owned())
}

fn json_version(path: &str) -> String {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("could not read {path} ({e})"));
    let value: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("could not parse {path} ({e})"));
    value
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{path} has no \"version\" string"))
        .to_owned()
}

fn assert_versions_agree() {
    let cargo = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is not set");
    let package = json_version("../package.json");
    let tauri_conf = json_version("tauri.conf.json");
    if package != cargo || tauri_conf != cargo {
        panic!(
            "version mismatch: src-tauri/Cargo.toml is {cargo}, package.json is {package}, src-tauri/tauri.conf.json is {tauri_conf}. Set all three to the same version so the installed launcher reports the version its installer shipped."
        );
    }
}
