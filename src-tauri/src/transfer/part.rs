// ------------ Part Files ------------
// Where a download lives until it is verified, and the record of which of its bytes are already on disk.

use std::path::{Path, PathBuf};

pub const IDENTITY_MIN_BYTES: u64 = 16 << 20;
const SIDECAR_V1: &str = "seg-v1";
const SIDECAR_V2: &str = "seg-v2";

pub fn with_suffix(file_path: &Path, suffix: &str) -> PathBuf {
    let mut os = file_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

pub fn part_path_for(file_path: &Path) -> PathBuf {
    with_suffix(file_path, ".part")
}

pub fn sidecar_path_for(file_path: &Path) -> PathBuf {
    with_suffix(file_path, ".part.segments.json")
}

pub fn identity_path_for(file_path: &Path) -> PathBuf {
    with_suffix(file_path, ".part.id.json")
}

pub fn tmp_path_for(path: &Path) -> PathBuf {
    with_suffix(path, ".tmp")
}

pub fn discard_part(file_path: &Path) {
    let _ = std::fs::remove_file(part_path_for(file_path));
    remove_sidecar(file_path);
    remove_identity(file_path);
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PartIdentity {
    pub size: u64,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
}

impl PartIdentity {
    pub fn expected(url: &str, size: u64, md5: &str) -> Self {
        let parsed = reqwest::Url::parse(url).ok();
        Self {
            size,
            md5: md5.to_ascii_lowercase(),
            path: parsed.as_ref().map(|u| u.path().to_string()).unwrap_or_default(),
            etag: None,
            last_modified: None,
            host: parsed
                .as_ref()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                .unwrap_or_default(),
        }
    }

    pub fn same_host(&self, other: &PartIdentity) -> bool {
        self.host.is_empty() || other.host.is_empty() || self.host == other.host
    }

    pub fn with_validators(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        let header = |name: reqwest::header::HeaderName| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        self.etag = header(reqwest::header::ETAG).filter(|e| !e.starts_with("W/"));
        self.last_modified = header(reqwest::header::LAST_MODIFIED);
        self
    }

    pub fn matches(&self, current: &PartIdentity) -> bool {
        let differs = |a: &Option<String>, b: &Option<String>| {
            matches!((a, b), (Some(a), Some(b)) if a != b)
        };
        self.size == current.size
            && self.md5.eq_ignore_ascii_case(&current.md5)
            && (!current.md5.is_empty()
                || (self.path == current.path
                    && (!self.same_host(current)
                        || (!differs(&self.etag, &current.etag)
                            && !differs(&self.last_modified, &current.last_modified)))))
    }

    pub fn if_range(&self) -> Option<&str> {
        self.etag.as_deref().or(self.last_modified.as_deref())
    }
}

pub fn records_identity(size: u64, md5: &str) -> bool {
    md5.is_empty() || size >= IDENTITY_MIN_BYTES
}

pub fn load_identity(file_path: &Path) -> Option<PartIdentity> {
    let text = std::fs::read_to_string(identity_path_for(file_path)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn remove_identity(file_path: &Path) {
    let path = identity_path_for(file_path);
    let _ = std::fs::remove_file(tmp_path_for(&path));
    let _ = std::fs::remove_file(path);
}

pub fn persist_json(path: &Path, value: &impl serde::Serialize) {
    let tmp = tmp_path_for(path);
    let Ok(text) = serde_json::to_string(value) else {
        return;
    };
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SegmentState {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SidecarFile {
    format: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    segments: Vec<SegmentState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    durable: Vec<Span>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<PartIdentity>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sidecar {
    pub size: u64,
    pub durable: Vec<Span>,
    pub identity: Option<PartIdentity>,
}

impl Sidecar {
    pub fn missing(&self) -> Vec<Span> {
        let mut out = Vec::new();
        let mut at = 0u64;
        for span in &self.durable {
            if span.start > at {
                out.push(Span { start: at, end: span.start });
            }
            at = at.max(span.end);
        }
        if at < self.size {
            out.push(Span { start: at, end: self.size });
        }
        out
    }
}

pub fn normalize(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|s| s.end > s.start);
    spans.sort_unstable_by_key(|s| s.start);
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => out.push(span),
        }
    }
    out
}

pub fn load_sidecar(file_path: &Path, expected_size: u64) -> Option<Sidecar> {
    let text = std::fs::read_to_string(sidecar_path_for(file_path)).ok()?;
    let file: SidecarFile = serde_json::from_str(&text).ok()?;
    if file.size != expected_size {
        return None;
    }
    let durable = match file.format.as_str() {
        SIDECAR_V2 => file.durable,
        SIDECAR_V1 => {
            if file.segments.is_empty()
                || !file
                    .segments
                    .iter()
                    .all(|s| s.start <= s.end && s.done <= s.end - s.start)
            {
                return None;
            }
            file.segments
                .iter()
                .map(|s| Span { start: s.start, end: s.start + s.done })
                .collect()
        }
        _ => return None,
    };
    if durable.iter().any(|s| s.start > s.end || s.end > expected_size) {
        return None;
    }
    Some(Sidecar {
        size: file.size,
        durable: normalize(durable),
        identity: file.identity,
    })
}

pub fn persist_sidecar(file_path: &Path, sidecar: &Sidecar) {
    let file = SidecarFile {
        format: SIDECAR_V2.to_string(),
        size: sidecar.size,
        segments: Vec::new(),
        durable: sidecar.durable.clone(),
        identity: sidecar.identity.clone(),
    };
    persist_json(&sidecar_path_for(file_path), &file);
}

pub fn remove_sidecar(file_path: &Path) {
    let path = sidecar_path_for(file_path);
    let _ = std::fs::remove_file(tmp_path_for(&path));
    let _ = std::fs::remove_file(path);
}

#[cfg(windows)]
pub fn set_sparse(file: &std::fs::File, sparse: bool) -> bool {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;

    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            in_buffer: *const c_void,
            in_size: u32,
            out_buffer: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    let flag: u8 = u8::from(sparse);
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            (&flag as *const u8).cast(),
            1,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        ) != 0
    }
}

#[cfg(not(windows))]
pub fn set_sparse(_file: &std::fs::File, _sparse: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_v1_sidecar_loads_as_the_ranges_it_claims() {
        let dir = crate::testing::scratch_dir("sidecar-v1");
        let file_path = dir.join("big.pak");
        std::fs::write(
            sidecar_path_for(&file_path),
            r#"{"format":"seg-v1","size":300,"segments":[{"start":0,"end":100,"done":40},{"start":100,"end":200,"done":100},{"start":200,"end":300,"done":0}]}"#,
        )
        .unwrap();
        let loaded = load_sidecar(&file_path, 300).unwrap();
        let wrong_size = load_sidecar(&file_path, 301);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            loaded.durable,
            vec![Span { start: 0, end: 40 }, Span { start: 100, end: 200 }]
        );
        assert_eq!(
            loaded.missing(),
            vec![Span { start: 40, end: 100 }, Span { start: 200, end: 300 }]
        );
        assert_eq!(loaded.identity, None, "a sidecar from before identities still loads");
        assert!(wrong_size.is_none());
    }

    #[test]
    fn a_v2_sidecar_round_trips_and_rejects_ranges_past_the_end() {
        let dir = crate::testing::scratch_dir("sidecar-v2");
        let file_path = dir.join("big.pak");
        let sidecar = Sidecar {
            size: 1000,
            durable: vec![Span { start: 0, end: 10 }, Span { start: 500, end: 600 }],
            identity: Some(PartIdentity::expected("https://h/a", 1000, "")),
        };
        persist_sidecar(&file_path, &sidecar);
        let loaded = load_sidecar(&file_path, 1000);
        std::fs::write(
            sidecar_path_for(&file_path),
            r#"{"format":"seg-v2","size":10,"durable":[{"start":0,"end":11}]}"#,
        )
        .unwrap();
        let past_end = load_sidecar(&file_path, 10);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(loaded, Some(sidecar));
        assert!(past_end.is_none());
    }

    #[test]
    fn overlapping_ranges_merge() {
        assert_eq!(
            normalize(vec![
                Span { start: 50, end: 60 },
                Span { start: 0, end: 10 },
                Span { start: 10, end: 20 },
                Span { start: 55, end: 70 },
                Span { start: 30, end: 30 },
            ]),
            vec![Span { start: 0, end: 20 }, Span { start: 50, end: 70 }]
        );
    }

    #[test]
    fn a_sparse_part_keeps_writes_in_place() {
        let dir = crate::testing::scratch_dir("sparse");
        let path = dir.join("x.part");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        set_sparse(&file, true);
        file.set_len(4 << 20).unwrap();
        crate::fsx::write_all_at(&file, b"tail", (4 << 20) - 4).unwrap();
        crate::fsx::write_all_at(&file, b"head", 0).unwrap();
        set_sparse(&file, false);
        drop(file);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(bytes.len(), 4 << 20);
        assert_eq!(&bytes[..4], b"head");
        assert_eq!(&bytes[bytes.len() - 4..], b"tail");
        assert!(bytes[4..bytes.len() - 4].iter().all(|b| *b == 0));
    }

    #[test]
    fn a_part_resumes_only_for_the_version_it_was_started_for() {
        let url = "https://cdn-a.example.com/game/1.2/Client/a.pak?sign=1";
        let recorded = PartIdentity::expected(url, 100, "ABCDEF");
        let same = PartIdentity::expected(
            "https://cdn-b.example.com/game/1.2/Client/a.pak?sign=2",
            100,
            "abcdef",
        );
        assert!(recorded.matches(&same), "a mirror or a new signature is the same file");
        assert!(!recorded.matches(&PartIdentity::expected(url, 100, "123456")));
        assert!(!recorded.matches(&PartIdentity::expected(url, 101, "ABCDEF")));

        let mut client = PartIdentity::expected("https://cdn.example.com/1.0/client.zip", 100, "");
        client.etag = Some("\"v1\"".to_string());
        let mut next = PartIdentity::expected("https://cdn.example.com/1.0/client.zip", 100, "");
        next.etag = Some("\"v2\"".to_string());
        assert!(!client.matches(&next));
        next.etag = None;
        assert!(client.matches(&next));
        assert!(!client.matches(&PartIdentity::expected(
            "https://cdn.example.com/1.1/client.zip",
            100,
            ""
        )));
        assert_eq!(client.if_range(), Some("\"v1\""));
    }

    #[test]
    fn a_part_without_a_checksum_resumes_on_another_mirror() {
        let mut started = PartIdentity::expected("https://a.example.com/1.6/full/x.hdiff", 100, "");
        started.etag = Some("\"from-a\"".to_string());
        let mut elsewhere = PartIdentity::expected("https://b.example.com/1.6/full/x.hdiff", 100, "");
        elsewhere.etag = Some("\"from-b\"".to_string());
        assert!(started.matches(&elsewhere), "mirrors give the same file different etags");
        assert!(!started.same_host(&elsewhere));
        assert!(!started.matches(&PartIdentity::expected("https://b.example.com/1.7/full/x.hdiff", 100, "")));

        let mut old = started.clone();
        old.host.clear();
        assert!(!old.matches(&elsewhere), "a record from before hosts were kept compares as before");
    }

    #[test]
    fn only_strong_etags_are_kept_for_if_range() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::ETAG, "W/\"weak\"".parse().unwrap());
        headers.insert(
            reqwest::header::LAST_MODIFIED,
            "Tue, 01 Sep 2026 10:00:00 GMT".parse().unwrap(),
        );
        let identity = PartIdentity::default().with_validators(&headers);
        assert_eq!(identity.etag, None);
        assert_eq!(identity.if_range(), Some("Tue, 01 Sep 2026 10:00:00 GMT"));
    }

    #[test]
    fn an_identity_survives_a_round_trip() {
        let dir = crate::testing::scratch_dir("identity");
        let file = dir.join("a.pak");
        let identity = PartIdentity::expected("https://cdn.example.com/a.pak", 64, "abc");
        persist_json(&identity_path_for(&file), &identity);
        let loaded = load_identity(&file);
        remove_identity(&file);
        let removed = load_identity(&file);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(loaded, Some(identity));
        assert_eq!(removed, None);
    }
}
