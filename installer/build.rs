// ------------ Installer Build Script ------------
// Gives setup.exe its icon, version info and manifest, and shrinks assets/wallpaper.jpg into the sharp and
// blurred JPEGs the setup window paints behind its panels.

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
  <asmv3:application xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <asmv3:windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
  <!-- Themed MessageBox buttons instead of the classic ones. -->
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;

const COVER_W: f32 = 1360.0;
const COVER_H: f32 = 880.0;
const JPEG_QUALITY: u8 = 80;
const WALLPAPER_SRC: &str = "assets/wallpaper.jpg";

fn write_jpeg(out_dir: &std::path::Path, name: &str, img: &image::RgbImage) {
    use image::codecs::jpeg::JpegEncoder;
    use image::ImageEncoder;

    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, JPEG_QUALITY)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap_or_else(|e| panic!("encode {name}: {e}"));
    std::fs::write(out_dir.join(name), &encoded).unwrap_or_else(|e| panic!("write {name}: {e}"));
}

fn prepare_wallpaper() {
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rerun-if-changed={WALLPAPER_SRC}");
    let img = image::open(WALLPAPER_SRC).unwrap_or_else(|e| panic!("read {WALLPAPER_SRC}: {e}"));
    let scale = (COVER_W / img.width() as f32).max(COVER_H / img.height() as f32);
    let img = if scale < 1.0 {
        img.resize(
            (img.width() as f32 * scale).ceil() as u32,
            (img.height() as f32 * scale).ceil() as u32,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

    let rgb = img.into_rgb8();
    write_jpeg(&out_dir, "wall.jpg", &rgb);

    let small = image::imageops::resize(
        &rgb,
        (rgb.width() / 3).max(1),
        (rgb.height() / 3).max(1),
        image::imageops::FilterType::Triangle,
    );
    let blurred = image::imageops::blur(&small, 6.0);
    write_jpeg(&out_dir, "wall-blur.jpg", &blurred);
}

fn numeric_version(version: &str) -> u64 {
    let core = version.split(['-', '+']).next().unwrap_or("");
    let mut parts = core
        .split('.')
        .map(|part| part.trim().parse::<u16>().unwrap_or(0) as u64);
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    (major << 48) | (minor << 32) | (patch << 16)
}

fn main() {
    println!("cargo:rerun-if-env-changed=PEEBIFY_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../src-tauri/icons/icon.ico");
    prepare_wallpaper();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/DEPENDENTLOADFLAG:0x800");
    }

    let version = std::env::var("PEEBIFY_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let mut res = tauri_winres::WindowsResource::new();
    res.set_icon("../src-tauri/icons/icon.ico");
    res.set("ProductName", "Peebify Launcher Setup");
    res.set("FileDescription", "Peebify Launcher Setup");
    res.set("CompanyName", "Peebify");
    res.set("LegalCopyright", "Copyright (c) Peebify");
    res.set("ProductVersion", &version);
    res.set("FileVersion", &version);
    let numeric = numeric_version(&version);
    res.set_version_info(tauri_winres::VersionInfo::FILEVERSION, numeric);
    res.set_version_info(tauri_winres::VersionInfo::PRODUCTVERSION, numeric);
    res.set_manifest(MANIFEST);
    if let Err(e) = res.compile() {
        panic!("winres failed to embed installer resources: {e}");
    }
}
