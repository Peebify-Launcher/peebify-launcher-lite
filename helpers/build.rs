// ------------ Helpers Build Script ------------
// Stamps each helper exe (FPS unlocker, mod loader, launch shim, overlay) with the Peebify icon and version info.
// The FPS unlocker, mod loader and launch shim ask for administrator rights, the overlay helper runs as a normal user.

const ADMIN_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 / 11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

const OVERLAY_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 / 11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <!-- Window rects must come back in true physical pixels, or every overlay
           position calculation is wrong on a scaled display. -->
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

const BINARIES: &[(&str, &str, &str)] = &[
    ("peebify-fps-helper", "Peebify FPS Unlocker", ADMIN_MANIFEST),
    ("peebify-mod-loader", "Peebify Mod Loader", ADMIN_MANIFEST),
    ("peebify-launch-shim", "Peebify Launch Shim", ADMIN_MANIFEST),
    ("peebify-overlay-helper", "Peebify Overlay", OVERLAY_MANIFEST),
];

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../src-tauri/icons/icon.ico");
    println!("cargo:rerun-if-env-changed=PEEBIFY_VERSION");
    let version = std::env::var("PEEBIFY_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let overlay = std::env::var_os("CARGO_FEATURE_OVERLAY_RUNTIME").is_some();

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    for &(binary, description, manifest) in BINARIES {
        if binary == "peebify-overlay-helper" && !overlay {
            continue;
        }
        if msvc && manifest == ADMIN_MANIFEST {
            println!("cargo:rustc-link-arg-bin={binary}=/DEPENDENTLOADFLAG:0x800");
        }
        let mut resources = tauri_winres::WindowsResource::new();
        resources.set_icon("../src-tauri/icons/icon.ico");
        resources.set("ProductName", "Peebify Launcher");
        resources.set("FileDescription", description);
        resources.set("CompanyName", "Peebify");
        resources.set("LegalCopyright", "Copyright (c) Peebify");
        resources.set("ProductVersion", &version);
        resources.set("FileVersion", &version);
        resources.set_manifest(manifest);

        let rc = out_dir.join(format!("{binary}.rc"));
        if let Err(error) = resources.write_resource_file(&rc) {
            panic!("failed to write {binary}'s resources: {error}");
        }
        embed_resource::compile_for(&rc, [binary], embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
