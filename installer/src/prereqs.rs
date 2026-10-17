// ------------ Runtime Prerequisites ------------
// Install-time checks for runtimes the launcher needs: Microsoft WebView2 always, the Visual C++ runtime on request.
// Missing ones are downloaded from Microsoft and only run if the file is signed by Microsoft Corporation.

use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::Cryptography::{CertGetNameStringW, CERT_NAME_SIMPLE_DISPLAY_TYPE};
use windows::Win32::Security::WinTrust::{
    WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

const TRUSTED_SIGNER: &str = "Microsoft Corporation";

fn lock_for_execution(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
}

fn signer_is_trusted(name: &str) -> bool {
    name == TRUSTED_SIGNER
}

unsafe fn signer_name(state: HANDLE) -> Option<String> {
    let provider = WTHelperProvDataFromStateData(state);
    if provider.is_null() {
        return None;
    }
    let signer = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
    if signer.is_null() {
        return None;
    }
    let cert = WTHelperGetProvCertFromChain(signer, 0);
    if cert.is_null() || (*cert).pCert.is_null() {
        return None;
    }
    let mut buf = [0u16; 256];
    let len = CertGetNameStringW(
        (*cert).pCert,
        CERT_NAME_SIMPLE_DISPLAY_TYPE,
        0,
        None,
        Some(&mut buf),
    ) as usize;
    if len <= 1 || len > buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len - 1]))
}

fn verify_signer(path: &Path, file: &File) -> Result<String, String> {
    let wide_path = HSTRING::from(path.as_os_str());
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide_path.as_ptr()),
        hFile: HANDLE(file.as_raw_handle()),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            &mut data as *mut WINTRUST_DATA as *mut core::ffi::c_void,
        )
    };
    let signer = if status == 0 {
        unsafe { signer_name(data.hWVTStateData) }
    } else {
        None
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            &mut data as *mut WINTRUST_DATA as *mut core::ffi::c_void,
        );
    }
    if status != 0 {
        return Err(format!(
            "its signature did not verify (0x{:08X})",
            status as u32
        ));
    }
    match signer {
        Some(name) if signer_is_trusted(&name) => Ok(name),
        Some(name) => Err(format!("it is signed by {name} instead of {TRUSTED_SIGNER}")),
        None => Err("its signer could not be read".into()),
    }
}

pub mod webview2 {
    use std::time::{Duration, Instant};

    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    use winreg::RegKey;

    use crate::ilog::ilog;
    use super::{lock_for_execution, net, verify_signer};
    use crate::win;

    const CLIENT_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    pub const BOOTSTRAPPER_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";
    const BOOTSTRAPPER_TIMEOUT: Duration = Duration::from_secs(20 * 60);

    pub fn is_installed() -> bool {
        let candidates: [(winreg::HKEY, String, u32); 3] = [
            (
                HKEY_LOCAL_MACHINE,
                format!(r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{CLIENT_GUID}"),
                KEY_READ,
            ),
            (
                HKEY_LOCAL_MACHINE,
                format!(r"SOFTWARE\Microsoft\EdgeUpdate\Clients\{CLIENT_GUID}"),
                KEY_READ | KEY_WOW64_64KEY,
            ),
            (
                HKEY_CURRENT_USER,
                format!(r"Software\Microsoft\EdgeUpdate\Clients\{CLIENT_GUID}"),
                KEY_READ,
            ),
        ];
        for (hive, path, flags) in candidates {
            if let Ok(key) = RegKey::predef(hive).open_subkey_with_flags(&path, flags) {
                if let Ok(pv) = key.get_value::<String, _>("pv") {
                    if !pv.is_empty() && pv != "0.0.0.0" {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn wait_for_bootstrapper(
        mut child: std::process::Child,
    ) -> Result<std::process::ExitStatus, String> {
        let deadline = Instant::now() + BOOTSTRAPPER_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Ok(None) => {
                    ilog!("webview2: bootstrapper still running after {BOOTSTRAPPER_TIMEOUT:?}");
                    return Err("The Microsoft WebView2 runtime installer is taking too long; \
                                setup stopped waiting for it"
                        .to_string());
                }
                Err(e) => return Err(format!("WebView2 bootstrapper could not be waited on: {e}")),
            }
        }
    }

    pub fn ensure(mut progress: impl FnMut(&str, f32)) -> Result<(), String> {
        if is_installed() {
            ilog!("webview2: runtime present");
            return Ok(());
        }
        ilog!("webview2: runtime missing, downloading bootstrapper");

        let dir = win::create_run_dir("peebify-webview2")
            .map_err(|e| format!("WebView2 download failed: {e}"))?;
        let tmp = dir.join("MicrosoftEdgeWebview2Setup.exe");
        let mut label = net::LabelThrottle::default();
        let downloaded = net::download_to_file(BOOTSTRAPPER_URL, &tmp, |done, total| {
            progress(
                label.label("Downloading the Microsoft WebView2 runtime", done, total),
                match total {
                    Some(total) => done as f32 / total as f32,
                    None => crate::msg::INDETERMINATE,
                },
            );
        });
        if let Err(e) = downloaded {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("WebView2 download failed: {e}"));
        }

        let lock = match lock_for_execution(&tmp) {
            Ok(lock) => lock,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(format!("WebView2 download could not be opened: {e}"));
            }
        };
        let signer = match verify_signer(&tmp, &lock) {
            Ok(signer) => signer,
            Err(e) => {
                drop(lock);
                let _ = std::fs::remove_dir_all(&dir);
                ilog!("webview2: download rejected because {e}");
                return Err(format!(
                    "The downloaded Microsoft WebView2 runtime was rejected because {e}"
                ));
            }
        };
        ilog!("webview2: signature verified, signed by {signer}");

        progress(
            "Installing the Microsoft WebView2 runtime…",
            crate::msg::INDETERMINATE,
        );
        ilog!("webview2: running bootstrapper");
        let status = std::process::Command::new(&tmp)
            .args(["/silent", "/install"])
            .spawn()
            .map_err(|e| format!("WebView2 bootstrapper failed to start: {e}"))
            .and_then(wait_for_bootstrapper);
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
        let status = status?;

        if is_installed() {
            ilog!("webview2: runtime installed");
            Ok(())
        } else {
            Err(format!(
                "WebView2 bootstrapper exited with {status} but the runtime is still not detected"
            ))
        }
    }
}

pub mod vcredist {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    use winreg::RegKey;

    use crate::ilog::ilog;
    use super::{lock_for_execution, net, verify_signer};
    use crate::win;

    const REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

    const RUNTIME_DLLS: [&str; 3] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];

    const RUNTIME_KEYS: [&str; 2] = [
        r"SOFTWARE\WOW6432Node\Microsoft\VisualStudio\14.0\VC\Runtimes\x64",
        r"SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64",
    ];

    const MIN_RUNTIME: (u32, u32) = (14, 40);

    pub(super) fn recent_enough(major: u32, minor: u32) -> bool {
        (major, minor) >= MIN_RUNTIME
    }

    fn dlls_present() -> bool {
        let dir = win::system32_dir();
        RUNTIME_DLLS.iter().all(|name| dir.join(name).is_file())
            && win::file_version(&dir.join("msvcp140.dll"))
                .is_some_and(|(major, minor, _)| recent_enough(major, minor))
    }

    fn registry_installed() -> bool {
        for path in RUNTIME_KEYS {
            let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
                .open_subkey_with_flags(path, KEY_READ | KEY_WOW64_64KEY)
            else {
                continue;
            };
            if key.get_value::<u32, _>("Installed").unwrap_or(0) != 1 {
                continue;
            }
            let major = key.get_value::<u32, _>("Major").unwrap_or(0);
            let minor = key.get_value::<u32, _>("Minor").unwrap_or(0);
            if recent_enough(major, minor) {
                return true;
            }
            ilog!(
                "vcredist: registered runtime {major}.{minor} is older than {}.{}",
                MIN_RUNTIME.0,
                MIN_RUNTIME.1
            );
        }
        false
    }

    pub fn is_installed() -> bool {
        dlls_present() || registry_installed()
    }

    pub fn ensure(mut progress: impl FnMut(&str, f32)) -> Result<(), String> {
        if is_installed() {
            ilog!("vcredist: runtime present");
            return Ok(());
        }
        ilog!("vcredist: runtime missing or out of date, downloading {REDIST_URL}");

        let dir = win::create_run_dir("peebify-vcredist")
            .map_err(|e| format!("Visual C++ runtime download failed: {e}"))?;
        let tmp = dir.join("vc_redist.x64.exe");
        let mut label = net::LabelThrottle::default();
        let downloaded = net::download_to_file(REDIST_URL, &tmp, |done, total| {
            progress(
                label.label("Downloading the Microsoft Visual C++ runtime", done, total),
                match total {
                    Some(total) => done as f32 / total as f32,
                    None => crate::msg::INDETERMINATE,
                },
            );
        });
        if let Err(e) = downloaded {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("Visual C++ runtime download failed: {e}"));
        }

        let lock = match lock_for_execution(&tmp) {
            Ok(lock) => lock,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(format!("Visual C++ runtime download could not be opened: {e}"));
            }
        };
        let signer = match verify_signer(&tmp, &lock) {
            Ok(signer) => signer,
            Err(e) => {
                drop(lock);
                let _ = std::fs::remove_dir_all(&dir);
                ilog!("vcredist: download rejected because {e}");
                return Err(format!(
                    "The downloaded Microsoft Visual C++ runtime was rejected because {e}. \
                     Install it manually from {REDIST_URL} if a game refuses to start."
                ));
            }
        };
        ilog!("vcredist: signature verified, signed by {signer}");

        progress(
            "Installing the Microsoft Visual C++ runtime…",
            crate::msg::INDETERMINATE,
        );
        ilog!("vcredist: running the redistributable (elevated)");
        let status = win::run_elevated_and_wait(&tmp, "/install /passive /norestart");
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);

        match status {
            Ok(0) | Ok(1638) => {
                ilog!("vcredist: runtime installed");
                Ok(())
            }
            Ok(3010) | Ok(1641) => {
                ilog!("vcredist: runtime installed, restart pending");
                Ok(())
            }
            Ok(code) => Err(format!(
                "The Microsoft Visual C++ runtime installer exited with {code}. \
                 Install it manually from {REDIST_URL} if a game refuses to start."
            )),
            Err(e) => Err(format!(
                "The Microsoft Visual C++ runtime was not installed ({e}). \
                 Install it from {REDIST_URL} if a game refuses to start."
            )),
        }
    }
}

pub(crate) mod net {
    use std::io::{Read, Write};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use crate::ilog::ilog;

    const ATTEMPTS: u32 = 3;
    const BUF_BYTES: usize = 64 * 1024;
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

    struct Agent {
        inner: ureq::Agent,
        read_idle: Duration,
    }

    fn agent(connect: Duration, read_idle: Duration) -> Agent {
        let inner = ureq::Agent::config_builder()
            .timeout_connect(Some(connect))
            .timeout_recv_response(Some(read_idle))
            .build()
            .into();
        Agent { inner, read_idle }
    }

    fn mb(bytes: u64) -> String {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }

    pub fn phase_label(label: &str, done: u64, total: Option<u64>) -> String {
        match total {
            Some(total) => format!("{label}: {} of {}", mb(done), mb(total)),
            None => format!("{label}: {}", mb(done)),
        }
    }

    const LABEL_INTERVAL: Duration = Duration::from_secs(1);

    #[derive(Default)]
    pub struct LabelThrottle {
        text: String,
        at: Option<Instant>,
    }

    impl LabelThrottle {
        pub fn label(&mut self, base: &str, done: u64, total: Option<u64>) -> &str {
            self.label_at(base, done, total, Instant::now())
        }

        fn label_at(&mut self, base: &str, done: u64, total: Option<u64>, now: Instant) -> &str {
            if self
                .at
                .is_none_or(|at| now.saturating_duration_since(at) >= LABEL_INTERVAL)
            {
                self.text = phase_label(base, done, total);
                self.at = Some(now);
            }
            &self.text
        }
    }

    pub fn download_to_file(
        url: &str,
        dest: &Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<(), String> {
        let agent = agent(CONNECT_TIMEOUT, READ_IDLE_TIMEOUT);
        let mut last_error = String::new();
        for attempt in 1..=ATTEMPTS {
            match attempt_download(&agent, url, dest, &mut progress) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    ilog!("download: attempt {attempt}/{ATTEMPTS} failed: {e}");
                    last_error = e;
                    let _ = std::fs::remove_file(dest);
                    if attempt < ATTEMPTS {
                        std::thread::sleep(Duration::from_secs(attempt as u64));
                    }
                }
            }
        }
        Err(last_error)
    }

    fn attempt_download(
        agent: &Agent,
        url: &str,
        dest: &Path,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Result<(), String> {
        let resp = agent.inner.get(url).call().map_err(|e| e.to_string())?;

        let total = resp
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|n| *n > 0);

        let mut reader = resp.into_body().into_reader();
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(4);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; BUF_BYTES];
            loop {
                let chunk = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) => Err(e),
                };
                let failed = chunk.is_err();
                if tx.send(chunk).is_err() || failed {
                    break;
                }
            }
        });

        let mut out = std::fs::File::create(dest).map_err(|e| e.to_string())?;
        let mut done: u64 = 0;
        progress(0, total);
        loop {
            let chunk = match rx.recv_timeout(agent.read_idle) {
                Ok(chunk) => chunk.map_err(|e| e.to_string())?,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "download stalled: no data for {} ms",
                        agent.read_idle.as_millis()
                    ))
                }
            };
            out.write_all(&chunk).map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        out.flush().map_err(|e| e.to_string())?;

        if let Some(total) = total {
            if done != total {
                return Err(format!("download ended early ({done} of {total} bytes)"));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::net::TcpListener;

        fn serve(chunks: usize, chunk: &'static [u8], gap: Duration) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut req = [0u8; 1024];
                let _ = stream.read(&mut req);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    chunks * chunk.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.flush();
                for _ in 0..chunks {
                    std::thread::sleep(gap);
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                }
            });
            format!("http://{addr}/file")
        }

        fn temp_dest(name: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!(
                "peebify-net-test-{}-{name}",
                std::process::id()
            ))
        }

        #[test]
        fn slow_body_longer_than_read_timeout_still_completes() {
            let url = serve(6, b"abcdefgh", Duration::from_millis(250));
            let dest = temp_dest("slow");
            let agent = agent(Duration::from_secs(5), Duration::from_secs(1));
            let mut last = (0, None);
            let result = attempt_download(&agent, &url, &dest, &mut |done, total| {
                last = (done, total)
            });
            let written = std::fs::read(&dest).unwrap_or_default();
            let _ = std::fs::remove_file(&dest);
            assert_eq!(result, Ok(()));
            assert_eq!(written.len(), 48);
            assert_eq!(last, (48, Some(48)));
        }

        #[test]
        fn download_label_changes_at_most_once_a_second() {
            let mut throttle = LabelThrottle::default();
            let start = Instant::now();
            let mb = 1024 * 1024;
            let mut labels = Vec::new();
            for step in 0..=250u64 {
                let now = start + Duration::from_millis(step * 20);
                let label = throttle.label_at("Downloading", step * mb / 10, Some(25 * mb), now);
                if labels.last() != Some(&label.to_string()) {
                    labels.push(label.to_string());
                }
            }
            assert_eq!(labels.len(), 6);
            assert_eq!(labels[0], "Downloading: 0.0 MB of 25.0 MB");
            assert_eq!(labels[1], "Downloading: 5.0 MB of 25.0 MB");
        }

        #[test]
        fn stalled_body_fails_on_read_idle_timeout() {
            let url = serve(2, b"abcdefgh", Duration::from_secs(3));
            let dest = temp_dest("stalled");
            let agent = agent(Duration::from_secs(5), Duration::from_millis(500));
            let started = std::time::Instant::now();
            let result = attempt_download(&agent, &url, &dest, &mut |_, _| {});
            let _ = std::fs::remove_file(&dest);
            assert!(result.is_err());
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win;

    #[test]
    fn runtimes_older_than_14_40_count_as_missing() {
        use vcredist::recent_enough;
        assert!(!recent_enough(14, 29));
        assert!(!recent_enough(14, 38));
        assert!(recent_enough(14, 40));
        assert!(recent_enough(14, 44));
        assert!(recent_enough(15, 0));
        assert!(!recent_enough(0, 0));
    }

    #[test]
    fn only_microsoft_corporation_is_trusted() {
        assert!(signer_is_trusted("Microsoft Corporation"));
        assert!(!signer_is_trusted("microsoft corporation"));
        assert!(!signer_is_trusted("Microsoft Corporation Ltd"));
        assert!(!signer_is_trusted(""));
    }

    #[test]
    fn locked_file_refuses_writers_and_deletion() {
        let dir = win::create_run_dir("peebify-prereqs-test").unwrap();
        let path = dir.join("probe.exe");
        std::fs::write(&path, b"MZ").unwrap();
        let lock = lock_for_execution(&path).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, dir.join("moved.exe")).is_err());
        assert!(lock_for_execution(&path).is_ok());
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsigned_file_is_rejected() {
        let dir = win::create_run_dir("peebify-prereqs-test").unwrap();
        let path = dir.join("unsigned.exe");
        std::fs::write(&path, b"MZ not a signed image").unwrap();
        let lock = lock_for_execution(&path).unwrap();
        assert!(verify_signer(&path, &lock).is_err());
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
