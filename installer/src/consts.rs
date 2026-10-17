// ------------ Installer Constants ------------
// Names and places shared by install and uninstall: product and binary names, the registry keys for the uninstall
// entry and start with Windows, the .staged and .backup swap folders, the helper exes and the default install folder.

pub const PRODUCT_NAME: &str = "Peebify Launcher";
pub const PUBLISHER: &str = "Peebify";
pub const MAIN_BINARY: &str = "Peebify Launcher.exe";
pub const APP_IDENTIFIER: &str = "com.peebify.launcher";
pub const DATA_DIR_NAME: &str = "Peebify Launcher";

pub const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Peebify Launcher";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "PeebifyLauncher";

pub const UNINSTALLER_NAME: &str = "uninstall.exe";
pub const INSTALL_MANIFEST_NAME: &str = "install-manifest.json";
pub const UPDATE_STATE_NAME: &str = ".update-state.json";
pub const STAGED_DIR_NAME: &str = ".staged";
pub const BACKUP_DIR_NAME: &str = ".backup";

pub const GAMES_DIR_NAME: &str = "games";

pub const RESOURCES_DIR_NAME: &str = "resources";
pub const HOOK_DLL_NAME: &str = "peebify_helpers.dll";
pub const HELPER_BINARIES: [&str; 3] = [
    "peebify-fps-helper.exe",
    "peebify-mod-loader.exe",
    "peebify-overlay-helper.exe",
];

pub const SHORTCUT_NAME: &str = "Peebify Launcher.lnk";

pub fn default_install_dir() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\"));
    base.join("Programs").join(PRODUCT_NAME)
}

pub fn user_data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|p| std::path::PathBuf::from(p).join(DATA_DIR_NAME))
}

pub fn app_cache_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| std::path::PathBuf::from(p).join(APP_IDENTIFIER))
}
