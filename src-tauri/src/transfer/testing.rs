// ------------ Test Helpers ------------
// Scratch folders and a local HTTP server whose files misbehave on cue, for the end-to-end tests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

pub fn scratch_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "peebify-transfer-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn hold_lock() -> parking_lot::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
}

#[derive(Clone, Debug, Default)]
pub struct Behavior {
    pub ignore_range: bool,
    pub cut_after: Option<(usize, u64)>,
    pub stall: Option<(u64, Duration)>,
    pub status: Option<(usize, u16)>,
    pub wrong_range_once: bool,
    pub drip: Option<u64>,
    pub max_concurrent: Option<usize>,
    pub no_etag: bool,
    pub etag_changes_after: Option<usize>,
}

struct Entry {
    body: Arc<Vec<u8>>,
    behavior: Behavior,
    requests: AtomicUsize,
    ranged: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
}

pub struct TestServer {
    pub base: String,
    files: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    peak_connections: Arc<AtomicUsize>,
}

impl TestServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let files: Arc<Mutex<HashMap<String, Arc<Entry>>>> = Arc::new(Mutex::new(HashMap::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let peak_connections = Arc::new(AtomicUsize::new(0));
        {
            let files = Arc::clone(&files);
            let connections = Arc::clone(&connections);
            let peak_connections = Arc::clone(&peak_connections);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let files = Arc::clone(&files);
                    let connections = Arc::clone(&connections);
                    let peak_connections = Arc::clone(&peak_connections);
                    std::thread::spawn(move || {
                        let now = connections.fetch_add(1, Ordering::SeqCst) + 1;
                        peak_connections.fetch_max(now, Ordering::SeqCst);
                        serve(stream, &files);
                        connections.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            });
        }
        Self {
            base,
            files,
            peak_connections,
        }
    }

    pub fn add(&self, path: &str, body: Vec<u8>, behavior: Behavior) -> String {
        self.files.lock().insert(
            path.to_string(),
            Arc::new(Entry {
                body: Arc::new(body),
                behavior,
                requests: AtomicUsize::new(0),
                ranged: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }),
        );
        format!("{}{path}", self.base)
    }

    fn entry(&self, path: &str) -> Arc<Entry> {
        Arc::clone(self.files.lock().get(path).expect("served path"))
    }

    pub fn requests(&self, path: &str) -> usize {
        self.entry(path).requests.load(Ordering::SeqCst)
    }

    pub fn ranged_requests(&self, path: &str) -> usize {
        self.entry(path).ranged.load(Ordering::SeqCst)
    }

    pub fn peak_requests(&self, path: &str) -> usize {
        self.entry(path).peak.load(Ordering::SeqCst)
    }

    pub fn peak_connections(&self) -> usize {
        self.peak_connections.load(Ordering::SeqCst)
    }
}

pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut state = 0x9E37_79B9u32 ^ u32::from(seed);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

struct Request {
    path: String,
    range: Option<(u64, Option<u64>)>,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut range = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("range") {
            let spec = value.trim().strip_prefix("bytes=")?;
            let (from, to) = spec.split_once('-')?;
            range = Some((from.parse().ok()?, to.parse().ok()));
        }
    }
    Some(Request { path, range })
}

fn serve(stream: TcpStream, files: &Mutex<HashMap<String, Arc<Entry>>>) {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    while let Some(request) = read_request(&mut reader) {
        let entry = files.lock().get(&request.path).cloned();
        let Some(entry) = entry else {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            continue;
        };
        let nth = entry.requests.fetch_add(1, Ordering::SeqCst);
        if request.range.is_some() {
            entry.ranged.fetch_add(1, Ordering::SeqCst);
        }
        let active = entry.active.fetch_add(1, Ordering::SeqCst) + 1;
        entry.peak.fetch_max(active, Ordering::SeqCst);
        let keep = respond(&mut stream, &entry, &request, nth, active);
        entry.active.fetch_sub(1, Ordering::SeqCst);
        if !keep {
            return;
        }
    }
    let _ = reader.read(&mut [0u8; 1]);
}

fn respond(stream: &mut TcpStream, entry: &Entry, request: &Request, nth: usize, active: usize) -> bool {
    let b = &entry.behavior;
    let body = &entry.body;
    if let Some(limit) = b.max_concurrent {
        if active > limit {
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
            return true;
        }
    }
    if let Some((first, code)) = b.status {
        if nth < first {
            let _ = stream.write_all(format!("HTTP/1.1 {code} Nope\r\nContent-Length: 0\r\n\r\n").as_bytes());
            return true;
        }
    }
    let etag = match b.etag_changes_after {
        Some(after) if nth >= after => "\"v2\"",
        _ => "\"v1\"",
    };
    let size = body.len() as u64;
    let (status, from, to) = match request.range {
        Some((from, to)) if !b.ignore_range => {
            if from >= size {
                let _ = stream.write_all(
                    format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\n\r\n")
                        .as_bytes(),
                );
                return true;
            }
            let to = to.unwrap_or(size - 1).min(size - 1);
            (206, from, to + 1)
        }
        _ => (200, 0, size),
    };
    let mut head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n",
        to - from
    );
    if !b.no_etag {
        head.push_str(&format!("ETag: {etag}\r\n"));
    }
    if status == 206 {
        let shown = if b.wrong_range_once && entry.ranged.load(Ordering::SeqCst) == 1 {
            from + 1
        } else {
            from
        };
        head.push_str(&format!("Content-Range: bytes {shown}-{}/{size}\r\n", to - 1));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return false;
    }
    let slice = &body[from as usize..to as usize];
    let cut = match b.cut_after {
        Some((times, after)) if nth < times => Some(after as usize),
        _ => None,
    };
    let mut stall = b.stall.filter(|_| nth == 0);
    let mut sent = 0usize;
    for piece in slice.chunks(16 << 10) {
        if let Some(after) = cut {
            if sent + piece.len() > after {
                let _ = stream.write_all(&piece[..after.saturating_sub(sent)]);
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return false;
            }
        }
        if let Some((after, quiet)) = stall {
            if sent as u64 >= after {
                let _ = stream.flush();
                std::thread::sleep(quiet);
                stall = None;
            }
        }
        if stream.write_all(piece).is_err() {
            return false;
        }
        sent += piece.len();
        if let Some(bps) = b.drip {
            std::thread::sleep(Duration::from_secs_f64(piece.len() as f64 / bps as f64));
        }
    }
    true
}
